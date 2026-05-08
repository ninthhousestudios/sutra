use std::collections::HashMap;

use serde_json::json;

use crate::db::Db;
use crate::error::{Result, SutraError};

pub fn handle(db: &Db, symbol: &str) -> Result<serde_json::Value> {
    let sym = db
        .resolve_symbol(symbol, None)?
        .ok_or_else(|| SutraError::NotFound {
            tool: "sutra_refs",
            kind: format!("symbol `{symbol}`"),
            next_action: "Use sutra_find to look up the symbol name first.".to_string(),
        })?;

    let all_refs = db.find_refs_to_symbol(sym.id)?;

    let unresolved_count = db
        .find_symbols_by_name(&sym.short_name, None, 1000)
        .ok()
        .map(|_| {
            all_refs
                .iter()
                .filter(|r| {
                    r.target_symbol_id.is_none()
                        && r.unresolved_name.as_deref() == Some(&sym.short_name)
                })
                .count()
        })
        .unwrap_or(0);

    let mut by_file: HashMap<i64, Vec<serde_json::Value>> = HashMap::new();
    for r in &all_refs {
        if r.target_symbol_id.is_some() {
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

    Ok(json!({
        "symbol": sym.qualified_name,
        "kind": sym.kind,
        "total_refs": resolved_count,
        "unresolved_candidates": unresolved_count,
        "references": references,
    }))
}
