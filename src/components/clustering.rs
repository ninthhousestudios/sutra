use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::db::{Db, FileRow};
use crate::error::Result;

use super::ComponentsConfig;

pub(super) type WeightedAdj = HashMap<i64, Vec<(i64, f64)>>;

pub(super) fn build_weighted_adjacency(files: &[FileRow], db: &Db) -> Result<(WeightedAdj, usize)> {
    let sym_to_file: HashMap<i64, i64> = db.all_symbol_file_map()?.into_iter().collect();
    let refs = db.all_resolved_refs()?;

    let mut directed: HashMap<(i64, i64), usize> = HashMap::new();
    for (src_file, target_sym) in &refs {
        if let Some(&target_file) = sym_to_file.get(target_sym)
            && *src_file != target_file
        {
            *directed.entry((*src_file, target_file)).or_default() += 1;
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
            if file_a.language == file_b.language && Path::new(&file_b.path).parent() == parent_a {
                *weight *= multiplier;
            }
        }
    }
}

pub(crate) fn is_test_file(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    segments
        .iter()
        .any(|s| matches!(*s, "test" | "tests" | "spec" | "__tests__"))
        || path.contains("_test.")
        || path.contains(".test.")
        || path.contains("_spec.")
        || path.contains(".spec.")
        || segments.last().is_some_and(|s| s.starts_with("test_"))
}

fn add_cochange_edges(
    adj: &mut WeightedAdj,
    db: &Db,
    files: &[FileRow],
    config: &ComponentsConfig,
) -> Result<usize> {
    let threshold = config
        .cochange_threshold
        .unwrap_or(super::DEFAULT_COCHANGE_THRESHOLD);
    let weight_scale = config
        .cochange_weight
        .unwrap_or(super::DEFAULT_COCHANGE_WEIGHT);

    let pairs = db.cochange_pairs_above_threshold(threshold)?;
    if pairs.is_empty() {
        return Ok(0);
    }

    let id_to_path: HashMap<i64, &str> = files.iter().map(|f| (f.id, f.path.as_str())).collect();

    let static_edges: HashSet<(i64, i64)> = adj
        .iter()
        .flat_map(|(&a, neighbors)| neighbors.iter().map(move |&(b, _)| (a.min(b), a.max(b))))
        .collect();

    let mut added = 0;
    for (fa, fb, jaccard, _shared) in pairs {
        let (lo, hi) = (fa.min(fb), fa.max(fb));
        if static_edges.contains(&(lo, hi)) {
            continue;
        }
        if let (Some(pa), Some(pb)) = (id_to_path.get(&fa), id_to_path.get(&fb))
            && is_test_file(pa) != is_test_file(pb)
        {
            continue;
        }
        let w = weight_scale * jaccard;
        adj.entry(fa).or_default().push((fb, w));
        adj.entry(fb).or_default().push((fa, w));
        added += 1;
    }
    Ok(added)
}

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

    let id_to_idx: HashMap<i64, usize> = nodes.iter().enumerate().map(|(i, &id)| (id, i)).collect();

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

    let mut sigma_tot: HashMap<usize, f64> = HashMap::new();
    for j in 0..n {
        *sigma_tot.entry(community[j]).or_default() += k[j];
    }

    let mut improved = true;
    while improved {
        improved = false;
        for i in 0..n {
            let current_comm = community[i];

            let mut comm_weights: HashMap<usize, f64> = HashMap::new();
            for &(j, w) in &neighbors[i] {
                *comm_weights.entry(community[j]).or_default() += w;
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
                let gain = (ki_in - ki_in_current) / m2
                    - resolution * k[i] * (sigma_c - sigma_current) / (m2 * m2);
                if gain > best_gain {
                    best_gain = gain;
                    best_comm = c;
                }
            }
            if best_comm != current_comm {
                *sigma_tot.entry(current_comm).or_default() -= k[i];
                *sigma_tot.entry(best_comm).or_default() += k[i];
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

fn auto_tune(adj: &WeightedAdj) -> LouvainResult {
    let n = adj.len();
    let candidates = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

    let results: Vec<LouvainResult> = candidates
        .iter()
        .map(|&gamma| louvain(adj, gamma))
        .collect();

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

pub(super) fn auto_name(paths: &[&str]) -> String {
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

    let parts: Vec<Vec<&str>> = paths
        .iter()
        .map(|p| p.split('/').collect::<Vec<_>>())
        .collect();

    // Longest common prefix (excluding filename components)
    let min_depth = parts
        .iter()
        .map(|p| p.len().saturating_sub(1))
        .min()
        .unwrap_or(0);
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

pub(super) fn build_clusters(result: &LouvainResult) -> Vec<Vec<i64>> {
    let mut by_comm: HashMap<usize, Vec<i64>> = HashMap::new();
    for (&file_id, &comm) in &result.communities {
        by_comm.entry(comm).or_default().push(file_id);
    }
    by_comm.into_values().collect()
}

#[allow(clippy::type_complexity)]
pub(super) fn run_clustering<'a>(
    db: &Db,
    files: &'a [FileRow],
    config: &ComponentsConfig,
    boundary_multipliers: &HashMap<String, f64>,
) -> Result<Option<(Vec<Vec<i64>>, HashMap<i64, &'a FileRow>, usize)>> {
    let (mut adj, edge_count) = build_weighted_adjacency(files, db)?;

    apply_boundary_hints(&mut adj, files, boundary_multipliers);
    add_cochange_edges(&mut adj, db, files, config)?;

    if !adj.values().any(|nbrs| !nbrs.is_empty()) {
        return Ok(None);
    }

    let result = if let Some(gamma) = config.resolution {
        louvain(&adj, gamma)
    } else {
        auto_tune(&adj)
    };

    let file_map: HashMap<i64, &FileRow> = files.iter().map(|f| (f.id, f)).collect();
    let clusters = build_clusters(&result);
    Ok(Some((clusters, file_map, edge_count)))
}

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

    #[test]
    fn test_is_test_file_directory_patterns() {
        assert!(is_test_file("tests/foo.rs"));
        assert!(is_test_file("src/test/helpers.rs"));
        assert!(is_test_file("spec/models/user_spec.rb"));
        assert!(is_test_file("src/__tests__/App.test.js"));
    }

    #[test]
    fn test_is_test_file_name_patterns() {
        assert!(is_test_file("src/foo_test.rs"));
        assert!(is_test_file("src/foo.test.ts"));
        assert!(is_test_file("src/foo_spec.rb"));
        assert!(is_test_file("src/foo.spec.ts"));
        assert!(is_test_file("test_helpers.py"));
    }

    #[test]
    fn test_is_test_file_non_test() {
        assert!(!is_test_file("src/main.rs"));
        assert!(!is_test_file("src/db/mod.rs"));
        assert!(!is_test_file("src/testing_utils.rs"));
        assert!(!is_test_file("src/attestation.rs"));
    }
}
