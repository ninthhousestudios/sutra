use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::db::{Db, FileRow, SymbolRow};
use crate::error::{Result, SutraError};
use crate::git;

const DEFAULT_STALENESS_THRESHOLD: f64 = 0.10;

fn to_json<T: serde::Serialize>(val: &T) -> String {
    serde_json::to_string(val).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ComponentsConfig {
    #[serde(default)]
    pub resolution: Option<f64>,
    #[serde(default)]
    pub staleness_threshold: Option<f64>,
}

pub fn load_config(root: &Path) -> Result<ComponentsConfig> {
    let path = root.join(".sutra/components.toml");
    if !path.exists() {
        return Ok(ComponentsConfig::default());
    }
    let content =
        std::fs::read_to_string(&path).map_err(|e| SutraError::Internal(format!("{e}")))?;
    toml::from_str(&content)
        .map_err(|e| SutraError::Internal(format!("components.toml parse error: {e}")))
}

pub fn parse_config(content: &str) -> Result<ComponentsConfig> {
    let mut config: ComponentsConfig = toml::from_str(content)
        .map_err(|e| SutraError::Internal(format!("components.toml parse error: {e}")))?;
    if let Some(t) = config.staleness_threshold {
        config.staleness_threshold = Some(t.clamp(0.0, 1.0));
    }
    Ok(config)
}

// ---------------------------------------------------------------------------
// Weighted adjacency
// ---------------------------------------------------------------------------

type WeightedAdj = HashMap<i64, Vec<(i64, f64)>>;

fn build_weighted_adjacency(files: &[FileRow], db: &Db) -> Result<(WeightedAdj, usize)> {
    let sym_to_file: HashMap<i64, i64> = db.all_symbol_file_map()?.into_iter().collect();
    let refs = db.all_resolved_refs()?;

    let mut directed: HashMap<(i64, i64), usize> = HashMap::new();
    for (src_file, target_sym) in &refs {
        if let Some(&target_file) = sym_to_file.get(target_sym) {
            if *src_file != target_file {
                *directed.entry((*src_file, target_file)).or_default() += 1;
            }
        }
    }

    // Symmetrize: canonical key = (min, max)
    let mut undirected: HashMap<(i64, i64), f64> = HashMap::new();
    for (&(a, b), &count) in &directed {
        let key = if a < b { (a, b) } else { (b, a) };
        // weight = count * (1/ambiguity); ambiguity=1 for all resolved refs
        *undirected.entry(key).or_default() += count as f64;
    }

    let edge_count = undirected.len();
    let mut adj: WeightedAdj = files.iter().map(|f| (f.id, Vec::new())).collect();
    for (&(a, b), &w) in &undirected {
        adj.entry(a).or_default().push((b, w));
        adj.entry(b).or_default().push((a, w));
    }
    Ok((adj, edge_count))
}

// ---------------------------------------------------------------------------
// Module boundary hints
// ---------------------------------------------------------------------------

fn apply_boundary_hints(
    adj: &mut WeightedAdj,
    files: &[FileRow],
    multipliers: &HashMap<String, f64>,
) {
    let file_map: HashMap<i64, &FileRow> = files.iter().map(|f| (f.id, f)).collect();

    for (&file_id, neighbors) in adj.iter_mut() {
        let Some(file_a) = file_map.get(&file_id) else {
            continue;
        };
        let multiplier = multipliers.get(&file_a.language).copied().unwrap_or(1.0);
        if multiplier == 1.0 {
            continue;
        }

        let parent_a = Path::new(&file_a.path).parent();

        for (nbr_id, weight) in neighbors.iter_mut() {
            let Some(file_b) = file_map.get(nbr_id) else {
                continue;
            };
            if file_a.language == file_b.language
                && Path::new(&file_b.path).parent() == parent_a
            {
                *weight *= multiplier;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Louvain community detection
// ---------------------------------------------------------------------------

pub struct LouvainResult {
    pub communities: HashMap<i64, usize>,
    pub modularity: f64,
    pub resolution: f64,
}

fn louvain(adj: &WeightedAdj, resolution: f64) -> LouvainResult {
    let nodes: Vec<i64> = adj.keys().copied().collect();
    let n = nodes.len();
    if n == 0 {
        return LouvainResult {
            communities: HashMap::new(),
            modularity: 0.0,
            resolution,
        };
    }

    let id_to_idx: HashMap<i64, usize> =
        nodes.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    let mut neighbors: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    let mut total_weight = 0.0;
    for (&node, edges) in adj {
        let i = id_to_idx[&node];
        for &(nbr, w) in edges {
            if let Some(&j) = id_to_idx.get(&nbr) {
                neighbors[i].push((j, w));
                total_weight += w;
            }
        }
    }
    // Each undirected edge counted twice
    total_weight /= 2.0;

    if total_weight == 0.0 {
        let communities = nodes.iter().enumerate().map(|(i, &id)| (id, i)).collect();
        return LouvainResult {
            communities,
            modularity: 0.0,
            resolution,
        };
    }

    let mut community: Vec<usize> = (0..n).collect();
    let mut k: Vec<f64> = vec![0.0; n];
    for i in 0..n {
        k[i] = neighbors[i].iter().map(|(_, w)| w).sum();
    }

    let m2 = 2.0 * total_weight;
    let mut improved = true;
    while improved {
        improved = false;
        for i in 0..n {
            let current_comm = community[i];

            let mut comm_weights: HashMap<usize, f64> = HashMap::new();
            for &(j, w) in &neighbors[i] {
                *comm_weights.entry(community[j]).or_default() += w;
            }

            let mut sigma_tot: HashMap<usize, f64> = HashMap::new();
            for j in 0..n {
                *sigma_tot.entry(community[j]).or_default() += k[j];
            }

            let ki_in_current = comm_weights.get(&current_comm).copied().unwrap_or(0.0);
            let sigma_current = sigma_tot.get(&current_comm).copied().unwrap_or(0.0) - k[i];

            let mut best_comm = current_comm;
            let mut best_gain = 0.0;
            for (&c, &ki_in) in &comm_weights {
                if c == current_comm {
                    continue;
                }
                let sigma_c = sigma_tot.get(&c).copied().unwrap_or(0.0);
                let gain =
                    (ki_in - ki_in_current) / m2 - resolution * k[i] * (sigma_c - sigma_current) / (m2 * m2);
                if gain > best_gain {
                    best_gain = gain;
                    best_comm = c;
                }
            }
            if best_comm != current_comm {
                community[i] = best_comm;
                improved = true;
            }
        }
    }

    // Compute modularity
    let mut q = 0.0;
    for i in 0..n {
        for &(j, w) in &neighbors[i] {
            if community[i] == community[j] {
                q += w - resolution * k[i] * k[j] / m2;
            }
        }
    }
    q /= m2;

    // Renumber communities to dense 0..k
    let mut remap: HashMap<usize, usize> = HashMap::new();
    let mut next = 0;
    let communities = nodes
        .iter()
        .map(|&id| {
            let c = community[id_to_idx[&id]];
            let mapped = *remap.entry(c).or_insert_with(|| {
                let v = next;
                next += 1;
                v
            });
            (id, mapped)
        })
        .collect();

    LouvainResult {
        communities,
        modularity: q,
        resolution,
    }
}

// ---------------------------------------------------------------------------
// Auto-tuning
// ---------------------------------------------------------------------------

fn auto_tune(adj: &WeightedAdj) -> LouvainResult {
    let n = adj.len();
    let candidates = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

    let results: Vec<LouvainResult> = candidates.iter().map(|&gamma| louvain(adj, gamma)).collect();

    let viable: Vec<&LouvainResult> = results
        .iter()
        .filter(|r| {
            let mut sizes: HashMap<usize, usize> = HashMap::new();
            for &c in r.communities.values() {
                *sizes.entry(c).or_default() += 1;
            }
            let k = sizes.len();
            let max_size = sizes.values().max().copied().unwrap_or(0);
            k >= 2 && (max_size as f64) <= 0.5 * n as f64
        })
        .collect();

    if let Some(best) = viable
        .iter()
        .max_by(|a, b| a.modularity.partial_cmp(&b.modularity).unwrap())
    {
        louvain(adj, best.resolution)
    } else {
        // All degenerate — fall back to γ=1.0
        louvain(adj, 1.0)
    }
}

// ---------------------------------------------------------------------------
// Auto-naming
// ---------------------------------------------------------------------------

fn auto_name(paths: &[&str]) -> String {
    if paths.is_empty() {
        return "unknown".to_string();
    }
    if paths.len() == 1 {
        return Path::new(paths[0])
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "root".to_string());
    }

    let parts: Vec<Vec<&str>> = paths.iter().map(|p| p.split('/').collect::<Vec<_>>()).collect();

    // Longest common prefix (excluding filename components)
    let min_depth = parts.iter().map(|p| p.len().saturating_sub(1)).min().unwrap_or(0);
    let mut prefix_len = 0;
    for i in 0..min_depth {
        if parts.iter().all(|p| p[i] == parts[0][i]) {
            prefix_len = i + 1;
        } else {
            break;
        }
    }

    if prefix_len == 0 {
        "root".to_string()
    } else {
        parts[0][prefix_len - 1].to_string()
    }
}

// ---------------------------------------------------------------------------
// Discovery orchestration
// ---------------------------------------------------------------------------

fn build_clusters(result: &LouvainResult) -> Vec<Vec<i64>> {
    let mut by_comm: HashMap<usize, Vec<i64>> = HashMap::new();
    for (&file_id, &comm) in &result.communities {
        by_comm.entry(comm).or_default().push(file_id);
    }
    by_comm.into_values().collect()
}

fn create_fresh_components(
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

fn reconcile_components(
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
        let paths_json =
            to_json(&cluster_paths[ki].iter().collect::<Vec<_>>());
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
        if contributors.len() >= 2 {
            if let Some(surviving_id) = cluster_to_comp.get(&ki) {
                let absorbed: Vec<_> = contributors
                    .iter()
                    .filter(|(ci, _)| !matched_comps.contains(ci))
                    .map(|(ci, _)| {
                        json!({"id": &existing[*ci].0, "name": &existing[*ci].1})
                    })
                    .collect();
                if !absorbed.is_empty() {
                    let detail = json!({ "absorbed": absorbed });
                    db.insert_component_event(
                        surviving_id,
                        "merge",
                        &detail.to_string(),
                    )?;
                }
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
                    cluster_to_comp.get(&ki).map(|comp_id| {
                        json!({"component_id": comp_id, "files": count})
                    })
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
            if let Some(&target_ki) = file_to_cluster.get(p.as_str()) {
                if target_ki != ki {
                    if let Some(target_id) = cluster_to_comp.get(&target_ki) {
                        *moved_to.entry(target_id).or_default() += 1;
                    }
                }
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

fn edge_count(files: &[FileRow], db: &Db) -> Result<usize> {
    let (_, count) = build_weighted_adjacency(files, db)?;
    Ok(count)
}

fn is_clustering_stale(
    db: &Db,
    current_edge_count: usize,
    current_file_count: i64,
    threshold: f64,
) -> Result<bool> {
    let Some((stored_edge_count, stored_file_count)) = db.clustering_meta()? else {
        return Ok(true);
    };

    if current_file_count != stored_file_count {
        return Ok(true);
    }

    if stored_edge_count == 0 {
        return Ok(current_edge_count > 0);
    }

    let delta = (current_edge_count as f64 - stored_edge_count as f64).abs();
    let ratio = delta / stored_edge_count as f64;
    Ok(ratio > threshold)
}

fn run_clustering<'a>(
    db: &Db,
    files: &'a [FileRow],
    config: &ComponentsConfig,
    boundary_multipliers: &HashMap<String, f64>,
) -> Result<Option<(Vec<Vec<i64>>, HashMap<i64, &'a FileRow>, usize)>> {
    let (mut adj, edge_count) = build_weighted_adjacency(files, db)?;
    if !adj.values().any(|nbrs| !nbrs.is_empty()) {
        return Ok(None);
    }

    apply_boundary_hints(&mut adj, files, boundary_multipliers);

    let result = if let Some(gamma) = config.resolution {
        louvain(&adj, gamma)
    } else {
        auto_tune(&adj)
    };

    let file_map: HashMap<i64, &FileRow> = files.iter().map(|f| (f.id, f)).collect();
    let clusters = build_clusters(&result);
    Ok(Some((clusters, file_map, edge_count)))
}

pub fn discover_components(
    db: &Db,
    files: &[FileRow],
    workspace_root: &Path,
    boundary_multipliers: &HashMap<String, f64>,
) -> Result<usize> {
    if files.is_empty() {
        return Ok(0);
    }

    let config = load_config(workspace_root)?;
    let threshold = config.staleness_threshold.unwrap_or(DEFAULT_STALENESS_THRESHOLD);
    let has_existing = db.component_count()? > 0;
    let has_membership = db.membership_count()? > 0;
    let file_count = files.len() as i64;

    if has_existing && has_membership {
        let current_edge_count = edge_count(files, db)?;
        if !is_clustering_stale(db, current_edge_count, file_count, threshold)? {
            return Ok(0);
        }
    }

    let Some((clusters, file_map, edge_count)) = run_clustering(db, files, &config, boundary_multipliers)? else {
        return Ok(0);
    };

    let count = if has_existing {
        reconcile_components(db, &clusters, &file_map)?
    } else {
        create_fresh_components(db, &clusters, &file_map)?
    };

    db.upsert_clustering_meta(edge_count as i64, file_count)?;
    Ok(count)
}

// ---------------------------------------------------------------------------
// Semantic anchors
// ---------------------------------------------------------------------------

pub const ANCHOR_KINDS: &[&str] = &[
    "function",
    "struct",
    "enum",
    "trait",
    "method",
    "type_alias",
    "const",
    "static",
];

pub fn anchor_count(eligible: usize) -> usize {
    (eligible / 8).clamp(3, 7)
}

fn tokenize_name(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in name.chars() {
        if ch == '_' || ch == '-' || ch == '/' {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current).to_lowercase());
            }
        } else if ch.is_uppercase() && !current.is_empty() {
            tokens.push(std::mem::take(&mut current).to_lowercase());
            current.push(ch);
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current.to_lowercase());
    }
    tokens
}

fn name_alignment(symbol_name: &str, component_name: &str) -> f64 {
    let sym_tokens: HashSet<String> = tokenize_name(symbol_name).into_iter().collect();
    let comp_tokens: HashSet<String> = tokenize_name(component_name).into_iter().collect();
    if sym_tokens.is_empty() || comp_tokens.is_empty() {
        return 0.0;
    }
    let intersection = sym_tokens.intersection(&comp_tokens).count();
    let union = sym_tokens.union(&comp_tokens).count();
    intersection as f64 / union as f64
}

fn rank_normalize(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    if n <= 1 {
        return vec![1.0; n];
    }
    let mut indexed: Vec<(usize, f64)> = values.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let mut ranks = vec![0.0; n];
    // Assign average rank to tied values
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j < n && (indexed[j].1 - indexed[i].1).abs() < f64::EPSILON {
            j += 1;
        }
        let avg_rank = (i + j - 1) as f64 / 2.0;
        for item in &indexed[i..j] {
            ranks[item.0] = avg_rank / (n - 1) as f64;
        }
        i = j;
    }
    ranks
}

struct ScoredSymbol {
    qualified_name: String,
    score: f64,
    rationale: String,
}

pub fn compute_semantic_anchors(db: &Db, workspace_root: &Path) -> Result<usize> {
    let components = db.all_components()?;
    if components.is_empty() {
        return Ok(0);
    }

    let sym_file_pairs = db.all_symbol_file_map()?;
    let sym_to_file: HashMap<i64, i64> = sym_file_pairs.into_iter().collect();
    let all_refs = db.all_resolved_refs()?;
    let churn_map = git::git_churn(workspace_root, 90).unwrap_or_default();

    let files = db.all_files()?;
    let file_id_to_path: HashMap<i64, &str> = files.iter().map(|f| (f.id, f.path.as_str())).collect();
    let all_symbols = db.all_symbols_by_file()?;

    let mut component_file_ids: HashMap<String, Vec<i64>> = HashMap::new();
    let mut file_to_component: HashMap<i64, String> = HashMap::new();
    for c in &components {
        let fids = db.component_file_ids(&c.id)?;
        for &fid in &fids {
            file_to_component.insert(fid, c.id.clone());
        }
        component_file_ids.insert(c.id.clone(), fids);
    }

    let mut intra_in_degree: HashMap<i64, usize> = HashMap::new();
    for &(src_file_id, target_sym_id) in &all_refs {
        if let Some(target_file_id) = sym_to_file.get(&target_sym_id) {
            let src_comp = file_to_component.get(&src_file_id);
            let tgt_comp = file_to_component.get(target_file_id);
            if src_comp.is_some() && src_comp == tgt_comp {
                *intra_in_degree.entry(target_sym_id).or_default() += 1;
            }
        }
    }

    let mut all_anchors = Vec::new();
    let mut total = 0;

    for c in &components {
        let fids = match component_file_ids.get(&c.id) {
            Some(f) => f,
            None => continue,
        };

        let eligible: Vec<&SymbolRow> = fids
            .iter()
            .filter_map(|fid| all_symbols.get(fid))
            .flatten()
            .filter(|s| ANCHOR_KINDS.contains(&s.kind.as_str()))
            .filter(|s| s.parent_symbol_id.is_none() || s.kind == "method")
            .collect();

        if eligible.is_empty() {
            continue;
        }

        let in_degrees: Vec<f64> = eligible
            .iter()
            .map(|s| intra_in_degree.get(&s.id).copied().unwrap_or(0) as f64)
            .collect();
        let pageranks: Vec<f64> = eligible
            .iter()
            .map(|s| s.pagerank.unwrap_or(0.0))
            .collect();
        let stabilities: Vec<f64> = eligible
            .iter()
            .map(|s| {
                let path = file_id_to_path.get(&s.file_id).copied().unwrap_or("");
                let churn = churn_map.get(path).copied().unwrap_or(0) as f64;
                1.0 / (1.0 + churn)
            })
            .collect();
        let namings: Vec<f64> = eligible
            .iter()
            .map(|s| name_alignment(&s.short_name, &c.name))
            .collect();

        let in_degree_norm = rank_normalize(&in_degrees);
        let pagerank_norm = rank_normalize(&pageranks);
        let stability_norm = rank_normalize(&stabilities);
        let naming_norm = rank_normalize(&namings);

        let mut scored: Vec<ScoredSymbol> = eligible
            .iter()
            .enumerate()
            .map(|(i, sym)| {
                let score = 0.35 * in_degree_norm[i]
                    + 0.30 * pagerank_norm[i]
                    + 0.20 * stability_norm[i]
                    + 0.15 * naming_norm[i];
                let rationale = format!(
                    "in_degree={:.2} pagerank={:.2} stability={:.2} naming={:.2}",
                    in_degree_norm[i], pagerank_norm[i], stability_norm[i], naming_norm[i],
                );
                ScoredSymbol {
                    qualified_name: sym.qualified_name.clone(),
                    score,
                    rationale,
                }
            })
            .collect();

        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        let n = anchor_count(eligible.len());
        scored.truncate(n);

        for s in &scored {
            all_anchors.push((
                Uuid::now_v7().to_string(),
                c.id.clone(),
                s.qualified_name.clone(),
                s.score,
                s.rationale.clone(),
            ));
        }
        total += scored.len();
    }

    db.replace_all_anchors(&all_anchors)?;
    Ok(total)
}

// ---------------------------------------------------------------------------
// Concept density
// ---------------------------------------------------------------------------

pub fn extract_stems(symbols: &[&SymbolRow]) -> usize {
    let mut stems: HashSet<String> = HashSet::new();
    for s in symbols {
        for token in tokenize_name(&s.short_name) {
            stems.insert(token);
        }
    }
    stems.len()
}

pub fn concept_density(symbols: &[&SymbolRow], total_loc: i64) -> f64 {
    if symbols.is_empty() || total_loc <= 0 {
        return 0.0;
    }
    let unique_kinds = symbols.iter().map(|s| s.kind.as_str()).collect::<HashSet<_>>().len();
    let stem_diversity = extract_stems(symbols);
    let raw = (unique_kinds as f64 * stem_diversity as f64) / total_loc as f64;
    (raw * 10000.0).round() / 10000.0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_name_single_file() {
        assert_eq!(auto_name(&["src/db/mod.rs"]), "db");
    }

    #[test]
    fn test_auto_name_shared_prefix() {
        assert_eq!(
            auto_name(&["src/tools/map.rs", "src/tools/outline.rs"]),
            "tools"
        );
    }

    #[test]
    fn test_auto_name_no_shared_prefix() {
        assert_eq!(auto_name(&["src/graph.rs", "tests/db-test.rs"]), "root");
    }

    #[test]
    fn test_auto_name_deep_prefix() {
        assert_eq!(
            auto_name(&["src/db/graph.rs", "src/db/migrations.rs", "src/db/mod.rs"]),
            "db"
        );
    }

    #[test]
    fn test_auto_name_empty() {
        assert_eq!(auto_name(&[]), "unknown");
    }

    #[test]
    fn test_parse_config_empty() {
        let c = parse_config("").unwrap();
        assert!(c.resolution.is_none());
    }

    #[test]
    fn test_parse_config_with_resolution() {
        let c = parse_config("resolution = 1.5").unwrap();
        assert_eq!(c.resolution, Some(1.5));
    }

    #[test]
    fn test_parse_config_with_staleness_threshold() {
        let c = parse_config("staleness_threshold = 0.25").unwrap();
        assert_eq!(c.staleness_threshold, Some(0.25));
    }

    #[test]
    fn test_parse_config_default_staleness_threshold() {
        let c = parse_config("resolution = 1.0").unwrap();
        assert!(c.staleness_threshold.is_none());
    }

    #[test]
    fn test_parse_config_staleness_threshold_clamped() {
        let c = parse_config("staleness_threshold = -0.5").unwrap();
        assert_eq!(c.staleness_threshold, Some(0.0));
        let c = parse_config("staleness_threshold = 2.0").unwrap();
        assert_eq!(c.staleness_threshold, Some(1.0));
    }

    #[test]
    fn test_louvain_two_cliques() {
        // Two disconnected cliques should produce two communities
        let mut adj: WeightedAdj = HashMap::new();
        // Clique A: nodes 1,2,3
        adj.insert(1, vec![(2, 5.0), (3, 5.0)]);
        adj.insert(2, vec![(1, 5.0), (3, 5.0)]);
        adj.insert(3, vec![(1, 5.0), (2, 5.0)]);
        // Clique B: nodes 4,5,6
        adj.insert(4, vec![(5, 5.0), (6, 5.0)]);
        adj.insert(5, vec![(4, 5.0), (6, 5.0)]);
        adj.insert(6, vec![(4, 5.0), (5, 5.0)]);

        let result = louvain(&adj, 1.0);

        let mut community_sets: HashMap<usize, Vec<i64>> = HashMap::new();
        for (&node, &comm) in &result.communities {
            community_sets.entry(comm).or_default().push(node);
        }
        assert_eq!(community_sets.len(), 2);

        // Nodes in clique A should share a community
        assert_eq!(result.communities[&1], result.communities[&2]);
        assert_eq!(result.communities[&2], result.communities[&3]);
        // Nodes in clique B should share a community
        assert_eq!(result.communities[&4], result.communities[&5]);
        assert_eq!(result.communities[&5], result.communities[&6]);
        // Different communities
        assert_ne!(result.communities[&1], result.communities[&4]);
    }

    #[test]
    fn test_louvain_single_node() {
        let mut adj: WeightedAdj = HashMap::new();
        adj.insert(1, vec![]);
        let result = louvain(&adj, 1.0);
        assert_eq!(result.communities.len(), 1);
    }

    #[test]
    fn test_auto_tune_picks_viable() {
        // Two cliques with a weak bridge — auto_tune should find 2 communities
        let mut adj: WeightedAdj = HashMap::new();
        adj.insert(1, vec![(2, 10.0), (3, 10.0), (4, 0.1)]);
        adj.insert(2, vec![(1, 10.0), (3, 10.0)]);
        adj.insert(3, vec![(1, 10.0), (2, 10.0)]);
        adj.insert(4, vec![(5, 10.0), (6, 10.0), (1, 0.1)]);
        adj.insert(5, vec![(4, 10.0), (6, 10.0)]);
        adj.insert(6, vec![(4, 10.0), (5, 10.0)]);

        let result = auto_tune(&adj);
        let mut community_sets: HashMap<usize, Vec<i64>> = HashMap::new();
        for (&node, &comm) in &result.communities {
            community_sets.entry(comm).or_default().push(node);
        }
        assert_eq!(community_sets.len(), 2);
    }

    fn make_symbol(id: i64, short_name: &str, kind: &str) -> SymbolRow {
        SymbolRow {
            id,
            file_id: 1,
            qualified_name: short_name.to_string(),
            short_name: short_name.to_string(),
            kind: kind.to_string(),
            signature: None,
            signature_hash: None,
            visibility: None,
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: None,
            pagerank: None,
            cyclomatic: None,
            cognitive: None,
            flags: 0,
            language_attrs: None,
        }
    }

    #[test]
    fn test_extract_stems_diverse() {
        let syms = vec![
            make_symbol(1, "UserProfile", "struct"),
            make_symbol(2, "fetch_data", "function"),
            make_symbol(3, "render_chart", "function"),
        ];
        let refs: Vec<&SymbolRow> = syms.iter().collect();
        let count = extract_stems(&refs);
        // user, profile, fetch, data, render, chart = 6
        assert_eq!(count, 6);
    }

    #[test]
    fn test_extract_stems_repetitive() {
        let syms = vec![
            make_symbol(1, "handle_create", "function"),
            make_symbol(2, "handle_update", "function"),
            make_symbol(3, "handle_delete", "function"),
        ];
        let refs: Vec<&SymbolRow> = syms.iter().collect();
        let count = extract_stems(&refs);
        // handle, create, update, delete = 4
        assert_eq!(count, 4);
    }

    #[test]
    fn test_concept_density_formula() {
        // 2 kinds (struct, function) × 6 stems / 100 LOC = 0.12
        let syms = vec![
            make_symbol(1, "UserProfile", "struct"),
            make_symbol(2, "fetch_data", "function"),
            make_symbol(3, "render_chart", "function"),
        ];
        let refs: Vec<&SymbolRow> = syms.iter().collect();
        let d = concept_density(&refs, 100);
        assert!((d - 0.12).abs() < 0.001, "expected 0.12, got {d}");
    }

    #[test]
    fn test_concept_density_empty() {
        let refs: Vec<&SymbolRow> = vec![];
        assert_eq!(concept_density(&refs, 100), 0.0);
    }

    #[test]
    fn test_concept_density_zero_loc() {
        let syms = vec![make_symbol(1, "foo", "function")];
        let refs: Vec<&SymbolRow> = syms.iter().collect();
        assert_eq!(concept_density(&refs, 0), 0.0);
    }
}
