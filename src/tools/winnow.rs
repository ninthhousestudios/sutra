use std::collections::HashSet;
use std::path::Path;

use regex::Regex;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::{self, FreshnessCounts};

pub struct WinnowFilter {
    pub kind: Option<String>,
    pub min_complexity: Option<i64>,
    pub min_churn: Option<u32>,
    pub churn_window_days: Option<u32>,
    pub calls_to: Option<String>,
    pub file_glob: Option<String>,
    pub name_regex: Option<String>,
    pub rank_by: Option<String>,
    pub limit: Option<i64>,
}

struct SymbolEntry {
    sym: crate::db::SymbolRow,
    file_path: String,
    file_last_parsed: String,
    complexity: i64,
    churn: u32,
    importance: f64,
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    filter: &WinnowFilter,
) -> Result<serde_json::Value> {
    let limit = filter.limit.unwrap_or(20) as usize;
    let files = db.all_files()?;

    let churn_map = if filter.min_churn.is_some() || filter.rank_by.as_deref() == Some("churn") {
        let window = filter.churn_window_days.unwrap_or(90);
        crate::git::git_churn(workspace_root, window).unwrap_or_default()
    } else {
        Default::default()
    };

    let glob_pattern = filter.file_glob.as_ref().map(|g| {
        glob::Pattern::new(g).unwrap_or_else(|_| glob::Pattern::new("*").unwrap())
    });

    let name_re = filter.name_regex.as_ref().and_then(|r| Regex::new(r).ok());

    let caller_ids: Option<HashSet<i64>> = if let Some(ref target_name) = filter.calls_to {
        let targets = db.find_symbols_by_name(target_name, None, 5)?;
        let mut ids = HashSet::new();
        for target in &targets {
            let refs = db.find_refs_to_symbol(target.id)?;
            for r in refs.iter().filter(|r| r.context_kind == "call") {
                if let Ok(Some(caller)) = db.find_enclosing_symbol(r.file_id, r.line) {
                    ids.insert(caller.id);
                }
            }
        }
        Some(ids)
    } else {
        None
    };

    let mut entries: Vec<SymbolEntry> = Vec::new();

    for file in &files {
        if let Some(ref pat) = glob_pattern {
            if !pat.matches(&file.path) {
                continue;
            }
        }

        let file_churn = churn_map.get(&file.path).copied().unwrap_or(0);
        if let Some(min) = filter.min_churn {
            if file_churn < min {
                continue;
            }
        }

        let symbols = db.find_symbols_by_file(file.id)?;
        for sym in symbols {
            if let Some(ref k) = filter.kind {
                if sym.kind != *k {
                    continue;
                }
            }

            let complexity = sym.cognitive.unwrap_or(0);
            if let Some(min) = filter.min_complexity {
                if complexity < min {
                    continue;
                }
            }

            if let Some(ref re) = name_re {
                if !re.is_match(&sym.qualified_name) && !re.is_match(&sym.short_name) {
                    continue;
                }
            }

            if let Some(ref ids) = caller_ids {
                if !ids.contains(&sym.id) {
                    continue;
                }
            }

            entries.push(SymbolEntry {
                importance: sym.pagerank.unwrap_or(0.0),
                complexity,
                churn: file_churn,
                file_path: file.path.clone(),
                file_last_parsed: file.last_parsed.clone(),
                sym,
            });
        }
    }

    match filter.rank_by.as_deref() {
        Some("complexity") => entries.sort_by_key(|e| std::cmp::Reverse(e.complexity)),
        Some("churn") => entries.sort_by_key(|e| std::cmp::Reverse(e.churn)),
        _ => entries.sort_by(|a, b| {
            b.importance
                .partial_cmp(&a.importance)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
    };

    entries.truncate(limit);

    let mut counts = FreshnessCounts::default();
    let items: Vec<_> = entries
        .iter()
        .map(|e| {
            let status = freshness::check_file(workspace_root, &e.file_path, &e.file_last_parsed);
            counts.record(status);
            json!({
                "qualified_name": e.sym.qualified_name,
                "short_name": e.sym.short_name,
                "kind": e.sym.kind,
                "file": e.file_path,
                "start_line": e.sym.start_line,
                "end_line": e.sym.end_line,
                "signature": e.sym.signature,
                "axes": {
                    "importance": e.importance,
                    "complexity": e.complexity,
                    "churn": e.churn,
                },
                "_freshness": status.as_str(),
            })
        })
        .collect();

    Ok(json!({
        "matches": items,
        "total": items.len(),
        "_meta": {
            "freshness": counts.to_json(),
        },
    }))
}
