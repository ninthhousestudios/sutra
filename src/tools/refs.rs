use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RefsArgs {
    pub workspace: String,
    pub symbol: String,
    /// Filter to refs with this context_kind (e.g. "construction", "call", "type_use")
    pub context_kind: Option<String>,
}
use crate::db::ResolveResult;
use crate::diagnostics::{CandidateInfo, Diagnostic};
use crate::error::Result;

pub fn handle(db: &Db, symbol: &str, context_kind: Option<&str>) -> Result<serde_json::Value> {
    let sym = match db.resolve_symbol_diagnostic(symbol, None)? {
        ResolveResult::Unique(s) => s,
        ResolveResult::NotFound => {
            return Ok(json!({
                "symbol": symbol,
                "diagnostic": serde_json::to_value(Diagnostic::NoSuchSymbol {
                    queried_name: symbol.to_string(),
                    queried_kind: None,
                    indexed_kinds: db.distinct_symbol_kinds().unwrap_or_default(),
                    suggestion: "Use sutra_find to search by partial name, \
                                 or sutra_grep for a text search.".to_string(),
                }).unwrap(),
            }));
        }
        ResolveResult::Ambiguous(candidates) => {
            let infos: Vec<CandidateInfo> = candidates
                .iter()
                .map(|c| CandidateInfo {
                    qualified_name: c.qualified_name.clone(),
                    kind: c.kind.clone(),
                    file: db
                        .file_by_id(c.file_id)
                        .ok()
                        .flatten()
                        .map(|f| f.path)
                        .unwrap_or_default(),
                })
                .collect();
            return Ok(json!({
                "symbol": symbol,
                "diagnostic": serde_json::to_value(Diagnostic::Ambiguous {
                    queried_name: symbol.to_string(),
                    candidates: infos,
                    suggestion: "Use the fully qualified name to disambiguate.".to_string(),
                }).unwrap(),
            }));
        }
    };

    let all_refs = db.find_refs_to_symbol(sym.id)?;

    let unresolved_count = all_refs
        .iter()
        .filter(|r| {
            r.target_symbol_id.is_none()
                && r.unresolved_name.as_deref() == Some(&*sym.short_name)
        })
        .count();

    let mut by_file: HashMap<i64, Vec<serde_json::Value>> = HashMap::new();
    for r in &all_refs {
        if r.target_symbol_id.is_some() {
            if let Some(ck) = context_kind
                && r.context_kind != ck
            {
                continue;
            }
            by_file.entry(r.file_id).or_default().push(json!({
                "line": r.line,
                "col": r.col,
                "context_kind": r.context_kind,
            }));
        }
    }

    let mut references: Vec<serde_json::Value> = by_file
        .into_iter()
        .filter_map(|(fid, locs)| {
            let path = db.file_by_id(fid).ok().flatten()?.path;
            Some(json!({
                "file": path,
                "locations": locs,
            }))
        })
        .collect();
    references.sort_by(|a, b| {
        a["file"]
            .as_str()
            .unwrap_or("")
            .cmp(b["file"].as_str().unwrap_or(""))
    });

    let resolved_count: usize = references
        .iter()
        .map(|r| r["locations"].as_array().map(|a| a.len()).unwrap_or(0))
        .sum();

    let mut result = json!({
        "symbol": sym.qualified_name,
        "kind": sym.kind,
        "total_refs": resolved_count,
        "unresolved_candidates": unresolved_count,
        "references": references,
    });

    if resolved_count == 0 && unresolved_count == 0 {
        result["diagnostic"] = serde_json::to_value(Diagnostic::SymbolExistsWithNoResults {
            symbol: sym.qualified_name.clone(),
            symbol_kind: sym.kind.clone(),
            tool: "sutra_refs".to_string(),
            suggestion: "The symbol exists but has no inbound references. \
                         It may be dead code, or references may be unresolved."
                .to_string(),
        })
        .unwrap();
    }

    Ok(result)
}
