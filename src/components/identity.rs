use std::collections::{HashMap, HashSet};

use serde_json::json;
use uuid::Uuid;

use crate::db::{Db, FileRow};
use crate::error::Result;

use super::clustering::{auto_name, build_weighted_adjacency};
use super::{
    ComponentsConfig, DEFAULT_COCHANGE_THRESHOLD, DEFAULT_COCHANGE_WEIGHT,
    DEFAULT_COCHANGE_WINDOW_DAYS, to_json,
};

pub(super) fn create_fresh_components(
    db: &Db,
    clusters: &[Vec<i64>],
    file_map: &HashMap<i64, &FileRow>,
) -> Result<usize> {
    let mut components = Vec::new();
    let mut membership = Vec::new();
    for fids in clusters {
        let paths: Vec<&str> = fids
            .iter()
            .filter_map(|id| file_map.get(id).map(|f| f.path.as_str()))
            .collect();
        let name = auto_name(&paths);
        let id = Uuid::now_v7().to_string();
        let paths_json = to_json(&paths);
        components.push((id.clone(), name, paths_json));
        for &fid in fids {
            membership.push((id.clone(), fid));
        }
    }
    db.batch_create_components(&components, &membership)?;
    Ok(clusters.len())
}

pub(super) fn reconcile_components(
    db: &Db,
    clusters: &[Vec<i64>],
    file_map: &HashMap<i64, &FileRow>,
) -> Result<usize> {
    let existing = db.active_components_with_paths()?;

    let cluster_paths: Vec<HashSet<String>> = clusters
        .iter()
        .map(|fids| {
            fids.iter()
                .filter_map(|id| file_map.get(id).map(|f| f.path.clone()))
                .collect()
        })
        .collect();

    let prior_sets: Vec<HashSet<&str>> = existing
        .iter()
        .map(|(_, _, paths)| paths.iter().map(|s| s.as_str()).collect())
        .collect();

    // Jaccard matrix + greedy bipartite matching
    let mut candidates: Vec<(usize, usize, f64)> = Vec::new();
    for (ci, prior_set) in prior_sets.iter().enumerate() {
        if prior_set.is_empty() {
            continue;
        }
        for (ki, cluster_set) in cluster_paths.iter().enumerate() {
            let inter = prior_set
                .iter()
                .filter(|p| cluster_set.contains(**p))
                .count();
            let union = prior_set.len() + cluster_set.len() - inter;
            if union > 0 {
                let j = inter as f64 / union as f64;
                if j >= 0.6 {
                    candidates.push((ci, ki, j));
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());

    let mut matched_comps: HashSet<usize> = HashSet::new();
    let mut matched_clusters: HashSet<usize> = HashSet::new();
    let mut matches: Vec<(usize, usize)> = Vec::new();
    for &(ci, ki, _) in &candidates {
        if !matched_comps.contains(&ci) && !matched_clusters.contains(&ki) {
            matches.push((ci, ki));
            matched_comps.insert(ci);
            matched_clusters.insert(ki);
        }
    }

    // Apply: clear membership, update matched, create new, dissolve unmatched
    db.clear_membership()?;
    let mut all_membership: Vec<(String, i64)> = Vec::new();

    for &(ci, ki) in &matches {
        let (id, _, _) = &existing[ci];
        let paths_json = to_json(&cluster_paths[ki].iter().collect::<Vec<_>>());
        db.update_component_paths(id, &paths_json)?;
        for &fid in &clusters[ki] {
            all_membership.push((id.clone(), fid));
        }
    }

    let mut new_components: Vec<(String, String, String)> = Vec::new();
    let mut new_cluster_to_comp: HashMap<usize, String> = HashMap::new();
    for (ki, fids) in clusters.iter().enumerate() {
        if matched_clusters.contains(&ki) {
            continue;
        }
        let paths: Vec<&str> = fids
            .iter()
            .filter_map(|id| file_map.get(id).map(|f| f.path.as_str()))
            .collect();
        let name = auto_name(&paths);
        let id = Uuid::now_v7().to_string();
        let paths_json = to_json(&paths);
        new_cluster_to_comp.insert(ki, id.clone());
        new_components.push((id.clone(), name, paths_json));
        for &fid in fids {
            all_membership.push((id.clone(), fid));
        }
    }

    for (ci, (id, _, _)) in existing.iter().enumerate() {
        if !matched_comps.contains(&ci) {
            db.dissolve_component(id)?;
        }
    }

    if !new_components.is_empty() {
        db.batch_create_components(&new_components, &all_membership)?;
    } else {
        db.batch_insert_membership(&all_membership)?;
    }

    detect_events(
        db,
        &existing,
        &cluster_paths,
        &matches,
        &matched_comps,
        &new_cluster_to_comp,
    )?;

    Ok(matches.len() + new_components.len())
}

fn detect_events(
    db: &Db,
    existing: &[(String, String, Vec<String>)],
    cluster_paths: &[HashSet<String>],
    matches: &[(usize, usize)],
    matched_comps: &HashSet<usize>,
    new_cluster_to_comp: &HashMap<usize, String>,
) -> Result<()> {
    let mut file_to_cluster: HashMap<&str, usize> = HashMap::new();
    for (ki, paths) in cluster_paths.iter().enumerate() {
        for p in paths {
            file_to_cluster.insert(p.as_str(), ki);
        }
    }

    // Map cluster → component ID (both matched and newly created)
    let mut cluster_to_comp: HashMap<usize, String> = HashMap::new();
    for &(ci, ki) in matches {
        cluster_to_comp.insert(ki, existing[ci].0.clone());
    }
    for (&ki, id) in new_cluster_to_comp {
        cluster_to_comp.insert(ki, id.clone());
    }

    // Merge: a cluster absorbed files from 2+ prior components,
    // but only count components that were dissolved (unmatched).
    for (ki, cset) in cluster_paths.iter().enumerate() {
        let mut contributors: Vec<(usize, usize)> = Vec::new();
        for (ci, (_, _, prior)) in existing.iter().enumerate() {
            let overlap = prior.iter().filter(|p| cset.contains(p.as_str())).count();
            if overlap > 0 {
                contributors.push((ci, overlap));
            }
        }
        if contributors.len() >= 2
            && let Some(surviving_id) = cluster_to_comp.get(&ki)
        {
            let absorbed: Vec<_> = contributors
                .iter()
                .filter(|(ci, _)| !matched_comps.contains(ci))
                .map(|(ci, _)| json!({"id": &existing[*ci].0, "name": &existing[*ci].1}))
                .collect();
            if !absorbed.is_empty() {
                let detail = json!({ "absorbed": absorbed });
                db.insert_component_event(surviving_id, "merge", &detail.to_string())?;
            }
        }
    }

    // Split: a component's prior files span 2+ clusters
    for (ci, (id, _, prior)) in existing.iter().enumerate() {
        if prior.is_empty() {
            continue;
        }
        let mut cluster_distribution: HashMap<usize, usize> = HashMap::new();
        for p in prior {
            if let Some(&target_ki) = file_to_cluster.get(p.as_str()) {
                *cluster_distribution.entry(target_ki).or_default() += 1;
            }
        }
        if cluster_distribution.len() >= 2 {
            let targets: Vec<_> = cluster_distribution
                .iter()
                .filter_map(|(&ki, &count)| {
                    cluster_to_comp
                        .get(&ki)
                        .map(|comp_id| json!({"component_id": comp_id, "files": count}))
                })
                .collect();
            if matched_comps.contains(&ci) {
                let matched_ki = matches.iter().find(|(mc, _)| *mc == ci).unwrap().1;
                let in_other = cluster_distribution
                    .iter()
                    .filter(|&(&k, _)| k != matched_ki)
                    .map(|(_, &v)| v)
                    .sum::<usize>();
                let detail = json!({
                    "files_in_matched_cluster": prior.len() - in_other,
                    "files_in_other_clusters": in_other,
                    "targets": targets,
                });
                db.insert_component_event(id, "split", &detail.to_string())?;
            } else {
                let detail = json!({
                    "clusters": cluster_distribution.len(),
                    "targets": targets,
                });
                db.insert_component_event(id, "split", &detail.to_string())?;
            }
        }
    }

    // Drift: >30% of a matched component's prior files now in a specific other component
    for &(ci, ki) in matches {
        let (id, _, prior) = &existing[ci];
        if prior.is_empty() {
            continue;
        }
        let mut moved_to: HashMap<&String, usize> = HashMap::new();
        for p in prior {
            if let Some(&target_ki) = file_to_cluster.get(p.as_str())
                && target_ki != ki
                && let Some(target_id) = cluster_to_comp.get(&target_ki)
            {
                *moved_to.entry(target_id).or_default() += 1;
            }
        }
        for (target_id, count) in moved_to {
            let ratio = count as f64 / prior.len() as f64;
            if ratio > 0.3 {
                let detail = json!({
                    "to_component": target_id,
                    "shifted_files": count,
                    "ratio": (ratio * 100.0).round() / 100.0,
                });
                db.insert_component_event(id, "drift", &detail.to_string())?;
            }
        }
    }

    Ok(())
}

pub(super) fn edge_count(files: &[FileRow], db: &Db) -> Result<usize> {
    let (_, count) = build_weighted_adjacency(files, db)?;
    Ok(count)
}

pub(super) fn clustering_config_hash(
    boundary_multipliers: &HashMap<String, f64>,
    config: &ComponentsConfig,
) -> String {
    use std::fmt::Write;
    let mut buf = String::new();
    if let Some(r) = config.resolution {
        write!(buf, "r={r};").unwrap();
    }
    let mut keys: Vec<_> = boundary_multipliers.keys().collect();
    keys.sort();
    for k in keys {
        write!(buf, "{k}={};", boundary_multipliers[k]).unwrap();
    }
    let ct = config
        .cochange_threshold
        .unwrap_or(DEFAULT_COCHANGE_THRESHOLD);
    let cw = config.cochange_weight.unwrap_or(DEFAULT_COCHANGE_WEIGHT);
    let cwd = config
        .cochange_window_days
        .unwrap_or(DEFAULT_COCHANGE_WINDOW_DAYS);
    write!(buf, "ct={ct};cw={cw};cwd={cwd};").unwrap();
    buf
}

pub(super) fn is_clustering_stale(
    db: &Db,
    current_edge_count: usize,
    current_file_count: i64,
    current_commit_file_count: i64,
    current_newest_commit_at: i64,
    threshold: f64,
    config_hash: &str,
) -> Result<bool> {
    let Some((stored_edge_count, stored_file_count, stored_hash, stored_cf_count, stored_newest)) =
        db.clustering_meta()?
    else {
        return Ok(true);
    };

    if stored_hash != config_hash {
        return Ok(true);
    }

    if current_file_count != stored_file_count {
        return Ok(true);
    }

    if current_newest_commit_at != stored_newest {
        return Ok(true);
    }

    if stored_edge_count == 0 {
        return Ok(current_edge_count > 0);
    }

    let edge_delta = (current_edge_count as f64 - stored_edge_count as f64).abs();
    let edge_ratio = edge_delta / stored_edge_count as f64;
    if edge_ratio > threshold {
        return Ok(true);
    }

    if stored_cf_count > 0 {
        let cf_delta = (current_commit_file_count as f64 - stored_cf_count as f64).abs();
        let cf_ratio = cf_delta / stored_cf_count as f64;
        if cf_ratio > threshold {
            return Ok(true);
        }
    } else if current_commit_file_count > 0 {
        return Ok(true);
    }

    Ok(false)
}
