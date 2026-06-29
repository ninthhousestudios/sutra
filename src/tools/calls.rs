use std::collections::{HashSet, VecDeque};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CallsArgs {
    pub workspace: String,
    pub symbol: String,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub depth: Option<usize>,
}
use crate::db::ResolveResult;
use crate::diagnostics::{CandidateInfo, Diagnostic};
use crate::error::Result;

const MAX_DEPTH: usize = 3;

pub fn handle(
    db: &Db,
    symbol: &str,
    direction: Option<&str>,
    depth: Option<usize>,
) -> Result<serde_json::Value> {
    let sym = match db.resolve_symbol_diagnostic(symbol, None)? {
        ResolveResult::Unique(s) => s,
        ResolveResult::NotFound => {
            return Ok(json!({
                "symbol": symbol,
                "diagnostic": serde_json::to_value(Diagnostic::NoSuchSymbol {
                    queried_name: symbol.to_string(),
                    queried_kind: None,
                    indexed_kinds: db.distinct_symbol_kinds().unwrap_or_default(),
                    freshness: None,
                    suggestion: "Use sutra_find to search by partial name, \
                                 or sutra_grep for a text search.".to_string(),
                }).unwrap(),
            }));
        }
        ResolveResult::Ambiguous(candidates) => {
            let infos: Vec<CandidateInfo> = candidates
                .iter()
                .map(|c| CandidateInfo {
                    qualified_name: c.qualified_name.to_string(),
                    kind: c.kind.to_string(),
                    file: db
                        .file_by_id(c.file_id)
                        .ok()
                        .flatten()
                        .map(|f| f.path.to_string())
                        .unwrap_or_default(),
                })
                .collect();
            return Ok(json!({
                "symbol": symbol,
                "diagnostic": serde_json::to_value(Diagnostic::Ambiguous {
                    queried_name: symbol.to_string(),
                    candidates: infos,
                    freshness: None,
                    suggestion: "Use the fully qualified name to disambiguate.".to_string(),
                }).unwrap(),
            }));
        }
    };

    let direction = direction.unwrap_or("callers");
    let depth = depth.unwrap_or(1).min(MAX_DEPTH);

    let entries = if direction == "callees" {
        collect_callees(db, &sym, depth)?
    } else {
        collect_callers(db, &sym, depth)?
    };

    Ok(json!({
        "symbol": sym.qualified_name,
        "kind": sym.kind,
        "direction": direction,
        "depth": depth,
        "entries": entries,
    }))
}

fn collect_callers(
    db: &Db,
    root: &crate::db::SymbolRow,
    depth: usize,
) -> Result<Vec<serde_json::Value>> {
    let mut visited = HashSet::new();
    visited.insert(root.id);
    let mut queue: VecDeque<(i64, usize)> = VecDeque::new();
    queue.push_back((root.id, 0));
    let mut entries = Vec::new();

    while let Some((sid, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        let refs = db.find_refs_to_symbol(sid)?;
        for r in refs.iter().filter(|r| r.context_kind == "call") {
            let file_path = db
                .file_by_id(r.file_id)
                .ok()
                .flatten()
                .map(|f| f.path.to_string())
                .unwrap_or_default();
            let caller_sym = db.find_enclosing_symbol(r.file_id, r.line)?;
            let caller_name = caller_sym
                .as_ref()
                .map(|s| &*s.qualified_name)
                .unwrap_or("<unknown>");
            entries.push(json!({
                "caller": caller_name,
                "file": file_path,
                "line": r.line,
                "depth": d + 1,
                "edge_type": &r.context_kind,
            }));
            if let Some(cs) = caller_sym
                && visited.insert(cs.id)
            {
                queue.push_back((cs.id, d + 1));
            }
        }
    }

    Ok(entries)
}

fn collect_callees(
    db: &Db,
    root: &crate::db::SymbolRow,
    depth: usize,
) -> Result<Vec<serde_json::Value>> {
    let mut visited = HashSet::new();
    visited.insert(root.id);
    let mut queue: VecDeque<(i64, i64, i64, usize)> = VecDeque::new();
    queue.push_back((root.id, root.file_id, root.start_line, 0));
    let mut entries = Vec::new();

    while let Some((sid, file_id, _start, d)) = queue.pop_front() {
        if d >= depth {
            continue;
        }
        let sym = match db.symbol_by_id(sid)? {
            Some(s) => s,
            None => continue,
        };
        let refs = db.find_refs_in_file(file_id)?;
        for r in refs.iter().filter(|r| {
            r.context_kind == "call" && r.line >= sym.start_line && r.line <= sym.end_line
        }) {
            if let Some(target_id) = r.target_symbol_id {
                let callee_sym = db.symbol_by_id(target_id)?;
                let callee_name = callee_sym
                    .as_ref()
                    .map(|s| &*s.qualified_name)
                    .unwrap_or_else(|| r.unresolved_name.as_deref().unwrap_or("<unknown>"));
                let callee_file = callee_sym
                    .as_ref()
                    .and_then(|s| db.file_by_id(s.file_id).ok().flatten())
                    .map(|f| f.path.to_string())
                    .unwrap_or_default();
                entries.push(json!({
                    "callee": callee_name,
                    "file": callee_file,
                    "line": r.line,
                    "depth": d + 1,
                    "edge_type": &r.context_kind,
                }));
                if let Some(cs) = callee_sym
                    && visited.insert(cs.id)
                {
                    queue.push_back((cs.id, cs.file_id, cs.start_line, d + 1));
                }
            } else if let Some(name) = &r.unresolved_name {
                entries.push(json!({
                    "callee": name,
                    "file": null,
                    "line": r.line,
                    "depth": d + 1,
                    "edge_type": &r.context_kind,
                    "unresolved": true,
                }));
            }
        }
    }

    Ok(entries)
}
