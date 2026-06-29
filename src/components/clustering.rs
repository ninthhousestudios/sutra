use std::collections::{HashMap, HashSet};
use std::path::Path;

use leiden_rs::{GraphDataBuilder, Leiden, LeidenConfig, QualityType};

use crate::db::{Db, FileRow};
use crate::error::Result;
use crate::graph::{EdgeKind, GraphData};

use super::ComponentsConfig;

pub(super) type WeightedAdj = HashMap<i64, Vec<(i64, f64)>>;

pub(super) fn build_weighted_adjacency(files: &[FileRow], gd: &GraphData) -> (WeightedAdj, usize) {
    let mut directed: HashMap<(i64, i64), f64> = HashMap::new();
    for &(src_file, target_sym, kind) in &gd.all_refs {
        if let Some(&target_file) = gd.sym_to_file.get(&target_sym)
            && src_file != target_file
        {
            *directed.entry((src_file, target_file)).or_default() += kind.clustering_weight();
        }
    }

    let import_weight = EdgeKind::Import.clustering_weight();
    for &(src, dst) in &gd.import_edges {
        if src != dst {
            *directed.entry((src, dst)).or_default() += import_weight;
        }
    }

    let mut undirected: HashMap<(i64, i64), f64> = HashMap::new();
    for (&(a, b), &weight) in &directed {
        let key = if a < b { (a, b) } else { (b, a) };
        *undirected.entry(key).or_default() += weight;
    }

    let edge_count = undirected.len();
    let mut adj: WeightedAdj = files.iter().map(|f| (f.id, Vec::new())).collect();
    for (&(a, b), &w) in &undirected {
        adj.entry(a).or_default().push((b, w));
        adj.entry(b).or_default().push((a, w));
    }
    (adj, edge_count)
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

        let parent_a = Path::new(&*file_a.path).parent();

        for (nbr_id, weight) in neighbors.iter_mut() {
            let Some(file_b) = file_map.get(nbr_id) else {
                continue;
            };
            if file_a.language == file_b.language && Path::new(&*file_b.path).parent() == parent_a {
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

    let id_to_path: HashMap<i64, &str> = files.iter().map(|f| (f.id, &*f.path)).collect();

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

pub struct ClusterResult {
    pub communities: HashMap<i64, usize>,
    pub modularity: f64,
    pub resolution: f64,
}

enum LeidenGraph {
    Empty,
    NoEdges {
        nodes: Vec<i64>,
    },
    Ready {
        nodes: Vec<i64>,
        graph: leiden_rs::GraphData,
    },
}

fn build_leiden_graph(adj: &WeightedAdj) -> LeidenGraph {
    let mut nodes: Vec<i64> = adj.keys().copied().collect();
    nodes.sort_unstable();
    let n = nodes.len();
    if n == 0 {
        return LeidenGraph::Empty;
    }

    let id_to_idx: HashMap<i64, usize> = nodes.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    let mut builder = GraphDataBuilder::new(n);
    let mut has_edges = false;
    for (&node, edges) in adj {
        let i = id_to_idx[&node];
        for &(nbr, w) in edges {
            if let Some(&j) = id_to_idx.get(&nbr) {
                if i < j {
                    builder.add_edge(i, j, w).unwrap();
                    has_edges = true;
                }
            }
        }
    }

    if !has_edges {
        return LeidenGraph::NoEdges { nodes };
    }

    LeidenGraph::Ready {
        nodes,
        graph: builder.build().unwrap(),
    }
}

fn run_leiden(lg: &LeidenGraph, resolution: f64) -> ClusterResult {
    run_leiden_with(lg, resolution, QualityType::Modularity)
}

fn run_leiden_with(lg: &LeidenGraph, resolution: f64, quality: QualityType) -> ClusterResult {
    match lg {
        LeidenGraph::Empty => ClusterResult {
            communities: HashMap::new(),
            modularity: 0.0,
            resolution,
        },
        LeidenGraph::NoEdges { nodes } => ClusterResult {
            communities: nodes.iter().enumerate().map(|(i, &id)| (id, i)).collect(),
            modularity: 0.0,
            resolution,
        },
        LeidenGraph::Ready { nodes, graph } => {
            let config = LeidenConfig::builder()
                .quality(quality)
                .resolution(resolution)
                .seed(42)
                .build();

            let output = Leiden::new(config).run(graph).unwrap();
            let membership = output.partition.as_slice();

            ClusterResult {
                communities: nodes
                    .iter()
                    .enumerate()
                    .map(|(idx, &id)| (id, membership[idx]))
                    .collect(),
                modularity: output.quality,
                resolution,
            }
        }
    }
}

fn auto_tune(lg: &LeidenGraph, node_count: usize) -> ClusterResult {
    let candidates = [0.5, 0.75, 1.0, 1.25, 1.5, 2.0];

    let results: Vec<ClusterResult> = candidates
        .iter()
        .map(|&gamma| run_leiden(lg, gamma))
        .collect();

    let viable: Vec<&ClusterResult> = results
        .iter()
        .filter(|r| {
            let mut sizes: HashMap<usize, usize> = HashMap::new();
            for &c in r.communities.values() {
                *sizes.entry(c).or_default() += 1;
            }
            let k = sizes.len();
            let max_size = sizes.values().max().copied().unwrap_or(0);
            k >= 2 && (max_size as f64) <= 0.5 * node_count as f64
        })
        .collect();

    if let Some(best) = viable
        .iter()
        .max_by(|a, b| a.modularity.partial_cmp(&b.modularity).unwrap())
    {
        run_leiden(lg, best.resolution)
    } else {
        run_leiden(lg, 1.0)
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

pub(super) const DEFAULT_MAX_COMMUNITY_SIZE: usize = 12;

fn split_oversized(result: &mut ClusterResult, adj: &WeightedAdj, max_size: usize) {
    let mut by_comm: HashMap<usize, Vec<i64>> = HashMap::new();
    for (&file_id, &comm) in &result.communities {
        by_comm.entry(comm).or_default().push(file_id);
    }

    let mut next_comm = result.communities.values().max().copied().unwrap_or(0) + 1;

    for (_, members) in &by_comm {
        if members.len() <= max_size {
            continue;
        }

        let member_set: HashSet<i64> = members.iter().copied().collect();

        // Extract the subgraph for this community
        let mut sub_adj: WeightedAdj = HashMap::new();
        for &node in members {
            let mut edges = Vec::new();
            if let Some(nbrs) = adj.get(&node) {
                for &(nbr, w) in nbrs {
                    if member_set.contains(&nbr) {
                        edges.push((nbr, w));
                    }
                }
            }
            sub_adj.insert(node, edges);
        }

        let sub_lg = build_leiden_graph(&sub_adj);

        let candidates = [2.0, 3.0, 5.0, 8.0];
        let mut accepted: Option<ClusterResult> = None;
        for &gamma in &candidates {
            let sub_result = run_leiden(&sub_lg, gamma);
            if community_count(&sub_result) <= 1 {
                continue;
            }
            if community_max_size(&sub_result) <= max_size {
                accepted = Some(sub_result);
                break;
            }
            if accepted.is_none() {
                accepted = Some(sub_result);
            }
        }

        // If Leiden couldn't split within budget, chunk deterministically
        let accepted = match accepted {
            Some(r) if community_max_size(&r) <= max_size => r,
            _ => {
                let mut sorted_members: Vec<i64> = members.clone();
                sorted_members.sort_unstable();
                let mut chunked = ClusterResult {
                    communities: HashMap::new(),
                    modularity: 0.0,
                    resolution: 0.0,
                };
                for (i, &id) in sorted_members.iter().enumerate() {
                    chunked.communities.insert(id, i / max_size);
                }
                chunked
            }
        };

        if community_count(&accepted) > 1 {
            for (&file_id, &sub_comm) in &accepted.communities {
                result.communities.insert(file_id, next_comm + sub_comm);
            }
            next_comm += community_count(&accepted);
        }
    }
}

fn community_max_size(result: &ClusterResult) -> usize {
    let mut sizes: HashMap<usize, usize> = HashMap::new();
    for &c in result.communities.values() {
        *sizes.entry(c).or_default() += 1;
    }
    sizes.values().max().copied().unwrap_or(0)
}

fn community_count(result: &ClusterResult) -> usize {
    let comms: HashSet<usize> = result.communities.values().copied().collect();
    comms.len()
}

pub(super) fn build_clusters(result: &ClusterResult) -> Vec<Vec<i64>> {
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
    gd: &GraphData,
    config: &ComponentsConfig,
    boundary_multipliers: &HashMap<String, f64>,
) -> Result<Option<(Vec<Vec<i64>>, HashMap<i64, &'a FileRow>, usize)>> {
    let (mut adj, edge_count) = build_weighted_adjacency(files, gd);

    apply_boundary_hints(&mut adj, files, boundary_multipliers);
    add_cochange_edges(&mut adj, db, files, config)?;

    if !adj.values().any(|nbrs| !nbrs.is_empty()) {
        return Ok(None);
    }

    let lg = build_leiden_graph(&adj);
    let mut result = if let Some(gamma) = config.resolution {
        run_leiden(&lg, gamma)
    } else {
        auto_tune(&lg, adj.len())
    };

    let max_size = config
        .max_community_size
        .unwrap_or(DEFAULT_MAX_COMMUNITY_SIZE);
    split_oversized(&mut result, &adj, max_size);

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
    fn test_leiden_two_cliques() {
        let mut adj: WeightedAdj = HashMap::new();
        adj.insert(1, vec![(2, 5.0), (3, 5.0)]);
        adj.insert(2, vec![(1, 5.0), (3, 5.0)]);
        adj.insert(3, vec![(1, 5.0), (2, 5.0)]);
        adj.insert(4, vec![(5, 5.0), (6, 5.0)]);
        adj.insert(5, vec![(4, 5.0), (6, 5.0)]);
        adj.insert(6, vec![(4, 5.0), (5, 5.0)]);

        let lg = build_leiden_graph(&adj);
        let result = run_leiden(&lg, 1.0);

        let mut community_sets: HashMap<usize, Vec<i64>> = HashMap::new();
        for (&node, &comm) in &result.communities {
            community_sets.entry(comm).or_default().push(node);
        }
        assert_eq!(community_sets.len(), 2);

        assert_eq!(result.communities[&1], result.communities[&2]);
        assert_eq!(result.communities[&2], result.communities[&3]);
        assert_eq!(result.communities[&4], result.communities[&5]);
        assert_eq!(result.communities[&5], result.communities[&6]);
        assert_ne!(result.communities[&1], result.communities[&4]);
    }

    #[test]
    fn test_leiden_single_node() {
        let mut adj: WeightedAdj = HashMap::new();
        adj.insert(1, vec![]);
        let lg = build_leiden_graph(&adj);
        let result = run_leiden(&lg, 1.0);
        assert_eq!(result.communities.len(), 1);
    }

    #[test]
    fn test_auto_tune_picks_viable() {
        let mut adj: WeightedAdj = HashMap::new();
        adj.insert(1, vec![(2, 10.0), (3, 10.0), (4, 0.1)]);
        adj.insert(2, vec![(1, 10.0), (3, 10.0)]);
        adj.insert(3, vec![(1, 10.0), (2, 10.0)]);
        adj.insert(4, vec![(5, 10.0), (6, 10.0), (1, 0.1)]);
        adj.insert(5, vec![(4, 10.0), (6, 10.0)]);
        adj.insert(6, vec![(4, 10.0), (5, 10.0)]);

        let lg = build_leiden_graph(&adj);
        let result = auto_tune(&lg, adj.len());
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

    #[test]
    fn test_leiden_deterministic_across_insertion_orders() {
        let make_graph = |order: &[i64]| -> WeightedAdj {
            let edges: HashMap<i64, Vec<(i64, f64)>> = HashMap::from([
                (1, vec![(2, 10.0), (3, 10.0), (4, 0.1)]),
                (2, vec![(1, 10.0), (3, 10.0)]),
                (3, vec![(1, 10.0), (2, 10.0)]),
                (4, vec![(5, 10.0), (6, 10.0), (1, 0.1)]),
                (5, vec![(4, 10.0), (6, 10.0)]),
                (6, vec![(4, 10.0), (5, 10.0)]),
            ]);
            let mut adj = WeightedAdj::new();
            for &id in order {
                adj.insert(id, edges[&id].clone());
            }
            adj
        };

        let orders: &[&[i64]] = &[
            &[1, 2, 3, 4, 5, 6],
            &[6, 5, 4, 3, 2, 1],
            &[3, 1, 5, 2, 6, 4],
            &[4, 6, 2, 5, 1, 3],
        ];

        let baseline_lg = build_leiden_graph(&make_graph(orders[0]));
        let baseline = run_leiden(&baseline_lg, 1.0);
        for order in &orders[1..] {
            let lg = build_leiden_graph(&make_graph(order));
            let result = run_leiden(&lg, 1.0);
            assert_eq!(
                baseline.communities, result.communities,
                "communities differ for insertion order {order:?}"
            );
        }
    }

    #[test]
    fn test_split_oversized_breaks_large_community() {
        // Two cliques of 4 connected by a weak bridge — Leiden puts them
        // together at low resolution. With max_size=4, split should separate.
        let mut adj: WeightedAdj = HashMap::new();
        // Clique A: nodes 1-4, strong internal edges
        for &a in &[1, 2, 3, 4] {
            let mut edges = Vec::new();
            for &b in &[1, 2, 3, 4] {
                if a != b {
                    edges.push((b, 10.0));
                }
            }
            adj.insert(a, edges);
        }
        // Clique B: nodes 5-8, strong internal edges
        for &a in &[5, 6, 7, 8] {
            let mut edges = Vec::new();
            for &b in &[5, 6, 7, 8] {
                if a != b {
                    edges.push((b, 10.0));
                }
            }
            adj.insert(a, edges);
        }
        // Weak bridge between cliques
        adj.get_mut(&4).unwrap().push((5, 1.0));
        adj.get_mut(&5).unwrap().push((4, 1.0));

        let lg = build_leiden_graph(&adj);
        let mut result = run_leiden(&lg, 0.5);

        split_oversized(&mut result, &adj, 4);

        let mut sizes_after: HashMap<usize, usize> = HashMap::new();
        for &c in result.communities.values() {
            *sizes_after.entry(c).or_default() += 1;
        }
        let max_after = *sizes_after.values().max().unwrap();

        assert!(
            max_after <= 4,
            "split must enforce max_size: got {max_after}"
        );
        assert!(
            sizes_after.len() >= 2,
            "split should produce at least 2 communities"
        );
        assert_eq!(result.communities.len(), 8);
    }

    #[test]
    fn test_split_oversized_leaves_small_communities() {
        let mut adj: WeightedAdj = HashMap::new();
        adj.insert(1, vec![(2, 5.0), (3, 5.0)]);
        adj.insert(2, vec![(1, 5.0), (3, 5.0)]);
        adj.insert(3, vec![(1, 5.0), (2, 5.0)]);

        let lg = build_leiden_graph(&adj);
        let mut result = run_leiden(&lg, 1.0);
        let before = result.communities.clone();

        split_oversized(&mut result, &adj, 10);

        assert_eq!(result.communities, before);
    }

    #[test]
    fn test_split_oversized_hub_with_spokes() {
        // Hub node 0 connected to 8 spokes, spokes not connected to each other.
        // This mimics the real problem (mcp.rs connected to many tools).
        let mut adj: WeightedAdj = HashMap::new();
        let spokes: Vec<i64> = (1..=8).collect();
        let mut hub_edges: Vec<(i64, f64)> = Vec::new();
        for &s in &spokes {
            hub_edges.push((s, 5.0));
            adj.insert(s, vec![(0, 5.0)]);
        }
        adj.insert(0, hub_edges);

        // Force all into one community
        let mut result = ClusterResult {
            communities: (0..=8).map(|id| (id, 0)).collect(),
            modularity: 0.5,
            resolution: 1.0,
        };

        split_oversized(&mut result, &adj, 4);

        let mut sizes: HashMap<usize, usize> = HashMap::new();
        for &c in result.communities.values() {
            *sizes.entry(c).or_default() += 1;
        }

        let max_size = *sizes.values().max().unwrap();
        assert!(
            max_size <= 4,
            "hub-spoke split must enforce max_size: got {max_size}"
        );
        assert!(sizes.len() > 1, "hub-spoke graph should be split");
        assert_eq!(result.communities.len(), 9, "all nodes still assigned");
    }
}
