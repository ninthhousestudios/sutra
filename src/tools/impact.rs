use std::collections::{HashSet, VecDeque};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpactArgs {
    pub workspace: String,
    pub symbol: String,
    /// When true, include `_explain` with BFS frontier by depth and threshold constants.
    #[serde(default)]
    pub explain: Option<bool>,
}
use crate::error::{Result, SutraError};

const BFS_MAX_DEPTH: usize = 3;

pub fn handle(db: &Db, symbol: &str, explain: bool) -> Result<serde_json::Value> {
    let sym = db
        .resolve_symbol(symbol, None)?
        .ok_or_else(|| SutraError::NotFound {
            tool: "sutra_impact",
            kind: format!("symbol `{symbol}`"),
            next_action: "Use sutra_find to look up the symbol name first.".to_string(),
        })?;

    let direct_refs = db.find_refs_to_symbol(sym.id)?;
    let direct_callers: Vec<_> = direct_refs
        .iter()
        .filter(|r| r.context_kind == "call")
        .collect();
    let direct_caller_count = direct_callers.len();

    let mut visited_files = HashSet::new();
    let mut visited_symbols = HashSet::new();
    visited_symbols.insert(sym.id);
    let mut queue: VecDeque<(i64, usize)> = VecDeque::new();
    queue.push_back((sym.id, 0));

    let mut frontier: Vec<Vec<serde_json::Value>> = vec![Vec::new(); BFS_MAX_DEPTH];

    while let Some((sid, depth)) = queue.pop_front() {
        if depth >= BFS_MAX_DEPTH {
            continue;
        }
        let refs = db.find_refs_to_symbol(sid)?;
        for r in &refs {
            visited_files.insert(r.file_id);
            if let Some(caller_sym) = db.find_enclosing_symbol(r.file_id, r.line)?
                && visited_symbols.insert(caller_sym.id)
            {
                if explain {
                    let file_path = db
                        .file_by_id(r.file_id)
                        .ok()
                        .flatten()
                        .map(|f| f.path)
                        .unwrap_or_default();
                    frontier[depth].push(json!({
                        "symbol": &caller_sym.qualified_name,
                        "file": file_path,
                    }));
                }
                queue.push_back((caller_sym.id, depth + 1));
            }
        }
    }

    let files_touched = visited_files.len();

    let (risk, mut risk_factors) = compute_risk(direct_caller_count, files_touched);

    let direct_files: HashSet<i64> = direct_refs.iter().map(|r| r.file_id).collect();
    let direct_file_paths: Vec<String> = direct_files
        .iter()
        .filter_map(|fid| db.file_by_id(*fid).ok().flatten().map(|f| f.path))
        .collect();

    if direct_file_paths.is_empty() && risk == "low" {
        risk_factors.push("no external callers found".to_string());
    }

    let sym_file_path = db
        .file_by_id(sym.file_id)?
        .map(|f| f.path)
        .unwrap_or_default();

    let mut result = json!({
        "symbol": sym.qualified_name,
        "kind": sym.kind,
        "file": sym_file_path,
        "direct_callers": direct_caller_count,
        "transitive_symbols": visited_symbols.len(),
        "files_touched": files_touched,
        "risk": risk,
        "risk_factors": risk_factors,
        "direct_caller_files": direct_file_paths,
    });

    if explain {
        result["_explain"] = json!({
            "bfs_frontier": {
                "depth_1": frontier[0],
                "depth_2": frontier[1],
                "depth_3": frontier[2],
            },
            "thresholds": {
                "high": { "direct_callers": 15, "files_touched": 20 },
                "medium": { "direct_callers": 5, "files_touched": 8 },
            }
        });
    }

    Ok(result)
}

fn compute_risk(direct_callers: usize, files_touched: usize) -> (&'static str, Vec<String>) {
    let mut factors = Vec::new();
    if direct_callers >= 15 {
        factors.push(format!("{direct_callers} direct callers (>=15 threshold)"));
    }
    if files_touched >= 20 {
        factors.push(format!("{files_touched} files touched (>=20 threshold)"));
    }
    if !factors.is_empty() {
        return ("high", factors);
    }
    if direct_callers >= 5 {
        factors.push(format!("{direct_callers} direct callers (>=5 threshold)"));
    }
    if files_touched >= 8 {
        factors.push(format!("{files_touched} files touched (>=8 threshold)"));
    }
    if !factors.is_empty() {
        return ("medium", factors);
    }
    ("low", factors)
}
