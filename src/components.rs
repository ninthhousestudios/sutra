use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use uuid::Uuid;

use crate::db::{Db, FileRow};
use crate::error::{Result, SutraError};

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ComponentsConfig {
    #[serde(default)]
    pub resolution: Option<f64>,
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
    toml::from_str(content)
        .map_err(|e| SutraError::Internal(format!("components.toml parse error: {e}")))
}

// ---------------------------------------------------------------------------
// Weighted adjacency
// ---------------------------------------------------------------------------

type WeightedAdj = HashMap<i64, Vec<(i64, f64)>>;

fn build_weighted_adjacency(files: &[FileRow], db: &Db) -> Result<WeightedAdj> {
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

    let mut adj: WeightedAdj = files.iter().map(|f| (f.id, Vec::new())).collect();
    for (&(a, b), &w) in &undirected {
        adj.entry(a).or_default().push((b, w));
        adj.entry(b).or_default().push((a, w));
    }
    Ok(adj)
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

pub fn discover_components(db: &Db, files: &[FileRow], workspace_root: &Path) -> Result<usize> {
    if files.is_empty() {
        return Ok(0);
    }

    if db.component_count()? > 0 {
        if db.membership_count()? > 0 {
            // Components and membership both present — reconciliation is sutra/46
            return Ok(0);
        }
        // Orphaned components (membership wiped by reindex) — clean up and re-discover
        db.delete_all_components()?;
    }

    let config = load_config(workspace_root)?;
    let adj = build_weighted_adjacency(files, db)?;

    let has_edges = adj.values().any(|nbrs| !nbrs.is_empty());
    if !has_edges {
        return Ok(0);
    }

    let result = if let Some(gamma) = config.resolution {
        louvain(&adj, gamma)
    } else {
        auto_tune(&adj)
    };

    let file_map: HashMap<i64, &FileRow> = files.iter().map(|f| (f.id, f)).collect();
    let mut clusters: HashMap<usize, Vec<i64>> = HashMap::new();
    for (&file_id, &comm) in &result.communities {
        clusters.entry(comm).or_default().push(file_id);
    }

    let mut components = Vec::new();
    let mut membership_rows = Vec::new();
    for file_ids in clusters.values() {
        let paths: Vec<&str> = file_ids
            .iter()
            .filter_map(|id| file_map.get(id).map(|f| f.path.as_str()))
            .collect();
        let name = auto_name(&paths);
        let id = Uuid::now_v7().to_string();
        let paths_json = serde_json::to_string(&paths).unwrap_or_default();

        components.push((id.clone(), name, paths_json));
        for &fid in file_ids {
            membership_rows.push((id.clone(), fid));
        }
    }
    db.batch_create_components(&components, &membership_rows)?;

    Ok(clusters.len())
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
}
