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
}
use crate::error::{Result, SutraError};

pub fn handle(db: &Db, path: Option<&str>, depth: Option<usize>) -> Result<serde_json::Value> {
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
