use std::collections::{HashMap, HashSet, VecDeque};

use crate::db::Db;
use crate::error::Result;

pub type FileGraph = HashMap<i64, HashSet<i64>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeKind {
    Call,
    Import,
    TypeUse,
    FieldAccess,
    Reference,
}

impl EdgeKind {
    pub fn from_context_kind(s: &str) -> Self {
        match s {
            "call" | "construction" => Self::Call,
            "import" => Self::Import,
            "type_use" => Self::TypeUse,
            "field_access" => Self::FieldAccess,
            _ => Self::Reference,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Call => "call",
            Self::Import => "import",
            Self::TypeUse => "type_use",
            Self::FieldAccess => "field_access",
            Self::Reference => "reference",
        }
    }

    pub fn clustering_weight(&self) -> f64 {
        match self {
            Self::Call => 1.0,
            Self::FieldAccess => 0.8,
            Self::TypeUse => 0.5,
            Self::Import => 0.3,
            Self::Reference => 0.2,
        }
    }
}

pub struct GraphData {
    pub sym_to_file: HashMap<i64, i64>,
    pub all_refs: Vec<(i64, i64, EdgeKind)>,
    pub import_edges: Vec<(i64, i64)>,
}

impl GraphData {
    pub fn load(db: &Db) -> Result<Self> {
        let sym_to_file = db.all_symbol_file_map()?.into_iter().collect();
        let raw_refs = db.all_resolved_refs()?;
        let all_refs = raw_refs
            .into_iter()
            .map(|(src, tgt, kind)| (src, tgt, EdgeKind::from_context_kind(&kind)))
            .collect();
        let import_edges = db.import_edges()?;
        Ok(Self {
            sym_to_file,
            all_refs,
            import_edges,
        })
    }
}

pub fn build_file_adjacency(
    files: &[crate::db::FileRow],
    gd: &GraphData,
) -> (FileGraph, FileGraph) {
    let mut fan_in_map: HashMap<i64, HashSet<i64>> = HashMap::new();
    let mut outgoing: HashMap<i64, HashSet<i64>> = HashMap::new();

    for f in files {
        fan_in_map.entry(f.id).or_default();
        outgoing.entry(f.id).or_default();
    }

    for &(src_file_id, target_sym_id, _) in &gd.all_refs {
        if let Some(&target_file_id) = gd.sym_to_file.get(&target_sym_id)
            && target_file_id != src_file_id
        {
            fan_in_map
                .entry(target_file_id)
                .or_default()
                .insert(src_file_id);
            outgoing
                .entry(src_file_id)
                .or_default()
                .insert(target_file_id);
        }
    }

    for &(src, dst) in &gd.import_edges {
        if src != dst {
            fan_in_map.entry(dst).or_default().insert(src);
            outgoing.entry(src).or_default().insert(dst);
        }
    }

    (fan_in_map, outgoing)
}

pub fn compute_rollups(
    db: &Db,
    files: &[crate::db::FileRow],
    changed_file_ids: Option<&HashSet<i64>>,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let gd = GraphData::load(db)?;
    let adjacency = build_file_adjacency(files, &gd);
    compute_rollups_with_adjacency(db, files, &adjacency, changed_file_ids)
}

pub fn compute_rollups_with_adjacency(
    db: &Db,
    files: &[crate::db::FileRow],
    adjacency: &(FileGraph, FileGraph),
    changed_file_ids: Option<&HashSet<i64>>,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let (fan_in_map, outgoing) = (&adjacency.0, &adjacency.1);

    let dirty: HashSet<i64> = match changed_file_ids {
        Some(changed) => {
            let mut dirty = changed.clone();
            for &fid in changed {
                if let Some(deps) = fan_in_map.get(&fid) {
                    dirty.extend(deps);
                }
                if let Some(targets) = outgoing.get(&fid) {
                    dirty.extend(targets);
                }
            }
            dirty
        }
        None => files.iter().map(|f| f.id).collect(),
    };

    for f in files {
        if !dirty.contains(&f.id) {
            continue;
        }
        let fan_in = fan_in_map.get(&f.id).map_or(0, |s| s.len()) as i64;
        let blast_radius = bfs_blast_radius(f.id, fan_in_map, 3) as i64;
        db.update_rollups(f.id, fan_in, blast_radius)?;
    }

    Ok(())
}

fn bfs_blast_radius(
    start: i64,
    fan_in_map: &HashMap<i64, HashSet<i64>>,
    max_depth: usize,
) -> usize {
    let mut visited: HashSet<i64> = HashSet::new();
    visited.insert(start);
    let mut queue: VecDeque<(i64, usize)> = VecDeque::new();

    if let Some(dependents) = fan_in_map.get(&start) {
        for &dep in dependents {
            if visited.insert(dep) {
                queue.push_back((dep, 1));
            }
        }
    }

    while let Some((file_id, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        if let Some(dependents) = fan_in_map.get(&file_id) {
            for &dep in dependents {
                if visited.insert(dep) {
                    queue.push_back((dep, depth + 1));
                }
            }
        }
    }

    visited.len() - 1
}

pub fn compute_pagerank(db: &Db, files: &[crate::db::FileRow]) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let gd = GraphData::load(db)?;
    let adjacency = build_file_adjacency(files, &gd);
    compute_pagerank_with_adjacency(db, files, &adjacency, &gd)
}

pub fn compute_pagerank_with_adjacency(
    db: &Db,
    files: &[crate::db::FileRow],
    adjacency: &(FileGraph, FileGraph),
    gd: &GraphData,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }

    let file_ids: Vec<i64> = files.iter().map(|f| f.id).collect();
    let n = file_ids.len();
    let id_to_idx: HashMap<i64, usize> = file_ids
        .iter()
        .enumerate()
        .map(|(i, &id)| (id, i))
        .collect();

    let (_, outgoing) = (&adjacency.0, &adjacency.1);
    let mut out_edges: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    for (&src_id, targets) in outgoing {
        if let Some(&si) = id_to_idx.get(&src_id) {
            for &dst_id in targets {
                if let Some(&di) = id_to_idx.get(&dst_id) {
                    out_edges[si].insert(di);
                }
            }
        }
    }

    for &(src, dst) in &gd.import_edges {
        if let (Some(&si), Some(&di)) = (id_to_idx.get(&src), id_to_idx.get(&dst))
            && si != di
        {
            out_edges[si].insert(di);
        }
    }

    const DAMPING: f64 = 0.85;
    const MAX_ITER: usize = 100;
    const EPSILON: f64 = 1e-6;

    let base = (1.0 - DAMPING) / n as f64;

    let has_existing = files.iter().any(|f| f.pagerank.is_some());
    let mut rank = if has_existing {
        let mut r: Vec<f64> = files
            .iter()
            .map(|f| f.pagerank.unwrap_or(1.0 / n as f64))
            .collect();
        let sum: f64 = r.iter().sum();
        if sum > 0.0 {
            for v in &mut r {
                *v /= sum;
            }
        }
        r
    } else {
        vec![1.0 / n as f64; n]
    };

    for _ in 0..MAX_ITER {
        let mut next = vec![base; n];
        for (src, edges) in out_edges.iter().enumerate() {
            if edges.is_empty() {
                let share = DAMPING * rank[src] / n as f64;
                for r in next.iter_mut() {
                    *r += share;
                }
            } else {
                let share = DAMPING * rank[src] / edges.len() as f64;
                for &dst in edges {
                    next[dst] += share;
                }
            }
        }

        let max_delta = rank
            .iter()
            .zip(next.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0_f64, f64::max);
        rank = next;
        if max_delta < EPSILON {
            break;
        }
    }

    let file_updates: Vec<(i64, f64)> = file_ids
        .iter()
        .enumerate()
        .map(|(i, &fid)| (fid, rank[i]))
        .collect();
    db.batch_update_file_pagerank(&file_updates)?;

    let mut ref_counts: HashMap<i64, usize> = HashMap::new();
    for &(_, target_sym_id, _) in &gd.all_refs {
        *ref_counts.entry(target_sym_id).or_default() += 1;
    }

    let mut file_symbols: HashMap<i64, Vec<(i64, usize)>> = HashMap::new();
    for (&sym_id, &file_id) in &gd.sym_to_file {
        let rc = ref_counts.get(&sym_id).copied().unwrap_or(0);
        file_symbols.entry(file_id).or_default().push((sym_id, rc));
    }

    let mut sym_updates: Vec<(i64, f64)> = Vec::new();
    for (&file_id, syms) in &file_symbols {
        let file_pr = rank[id_to_idx[&file_id]];
        let total_incoming: usize = syms.iter().map(|(_, rc)| rc).sum();
        for &(sym_id, sym_refs) in syms {
            let sym_pr = if total_incoming > 0 {
                file_pr * sym_refs as f64 / total_incoming as f64
            } else {
                file_pr / syms.len().max(1) as f64
            };
            sym_updates.push((sym_id, sym_pr));
        }
    }
    db.batch_update_symbol_pagerank(&sym_updates)?;

    Ok(())
}
