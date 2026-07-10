use std::collections::{HashMap, HashSet, VecDeque};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DepsArgs {
    pub workspace: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub depth: Option<usize>,
    #[serde(default)]
    pub cycles: Option<bool>,
}
use crate::error::{Result, SutraError};

pub fn handle(
    db: &Db,
    path: Option<&str>,
    depth: Option<usize>,
    cycles: bool,
) -> Result<serde_json::Value> {
    if cycles {
        return handle_cycles(db, path);
    }
    let depth = depth.unwrap_or(2);

    if let Some(path) = path {
        let file = db.file_by_path(path)?.ok_or_else(|| SutraError::NotFound {
            tool: "sutra_deps",
            kind: format!("file `{path}`"),
            next_action: "Check the path. Use sutra_map to list available files.".to_string(),
        })?;

        let all_edges = db.import_edges()?;
        let mut adj: HashMap<i64, Vec<i64>> = HashMap::new();
        for (from, to) in &all_edges {
            adj.entry(*from).or_default().push(*to);
        }

        let mut visited = HashSet::new();
        visited.insert(file.id);
        let mut queue = VecDeque::new();
        queue.push_back((file.id, 0usize));
        let mut edges = Vec::new();

        while let Some((fid, d)) = queue.pop_front() {
            if d >= depth {
                continue;
            }
            if let Some(targets) = adj.get(&fid) {
                for &tid in targets {
                    let from_path = db.file_by_id(fid).ok().flatten().map(|f| f.path);
                    let to_path = db.file_by_id(tid).ok().flatten().map(|f| f.path);
                    edges.push(json!({
                        "from": from_path,
                        "to": to_path,
                    }));
                    if visited.insert(tid) {
                        queue.push_back((tid, d + 1));
                    }
                }
            }
        }

        let nodes: Vec<_> = visited
            .iter()
            .filter_map(|fid| db.file_by_id(*fid).ok().flatten().map(|f| f.path))
            .collect();

        Ok(json!({
            "root": path,
            "depth": depth,
            "nodes": nodes,
            "edges": edges,
        }))
    } else {
        let all_edges = db.import_edges()?;
        let edges: Vec<_> = all_edges
            .iter()
            .filter_map(|(from, to)| {
                let from_path = db.file_by_id(*from).ok().flatten().map(|f| f.path)?;
                let to_path = db.file_by_id(*to).ok().flatten().map(|f| f.path)?;
                Some(json!({ "from": from_path, "to": to_path }))
            })
            .collect();

        Ok(json!({
            "depth": "all",
            "edges": edges,
            "total_edges": edges.len(),
        }))
    }
}

fn handle_cycles(db: &Db, path: Option<&str>) -> Result<serde_json::Value> {
    let all_edges = db.import_edges()?;
    let sccs = crate::graph::find_import_sccs(&all_edges);

    let filter_id = if let Some(p) = path {
        Some(
            db.file_by_path(p)?
                .ok_or_else(|| SutraError::NotFound {
                    tool: "sutra_deps",
                    kind: format!("file `{p}`"),
                    next_action: "Check the path. Use sutra_map to list available files."
                        .to_string(),
                })?
                .id,
        )
    } else {
        None
    };

    let mut cycles: Vec<serde_json::Value> = Vec::new();
    let mut files_in_cycles: HashSet<i64> = HashSet::new();

    for scc in &sccs {
        if let Some(fid) = filter_id
            && !scc.contains(&fid)
        {
            continue;
        }
        let paths: Vec<_> = scc
            .iter()
            .filter_map(|id| db.file_by_id(*id).ok().flatten().map(|f| f.path))
            .collect();
        files_in_cycles.extend(scc);
        cycles.push(json!(paths));
    }

    Ok(json!({
        "cycles": cycles,
        "total_sccs": cycles.len(),
        "files_in_cycles": files_in_cycles.len(),
    }))
}
