use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use regex::Regex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, FileRow};
use crate::error::Result;
use crate::freshness::{self, FreshnessCounts};

type FilePredicate = Box<dyn Fn(&FileRow) -> bool>;
type SymbolPredicate = Box<dyn Fn(&SymbolEntry) -> bool>;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WinnowArgs {
    pub workspace: String,
    /// Filter by symbol kind (function, method, struct, etc.)
    #[serde(default)]
    pub kind: Option<String>,
    /// Minimum cognitive complexity
    #[serde(default)]
    pub min_complexity: Option<i64>,
    /// Minimum git churn (commit count in window)
    #[serde(default)]
    pub min_churn: Option<u32>,
    /// Churn window in days (default 90)
    #[serde(default)]
    pub churn_window_days: Option<u32>,
    /// Symbol name that results must call
    #[serde(default)]
    pub calls_to: Option<String>,
    /// Glob pattern for file paths
    #[serde(default)]
    pub file_glob: Option<String>,
    /// Regex for symbol names (matched against qualified_name and short_name)
    #[serde(default)]
    pub name_regex: Option<String>,
    /// Rank results by: "importance" (default), "complexity", "churn"
    #[serde(default)]
    pub rank_by: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

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

// --- File-level predicate builders ---

fn glob_filter(pattern: &str) -> FilePredicate {
    let pat = glob::Pattern::new(pattern).unwrap_or_else(|_| glob::Pattern::new("*").unwrap());
    Box::new(move |file| pat.matches(&file.path))
}

fn churn_filter(churn_map: HashMap<String, u32>, min: u32) -> FilePredicate {
    Box::new(move |file| churn_map.get(&*file.path).copied().unwrap_or(0) >= min)
}

// --- Symbol-level predicate builders ---

fn kind_filter(kind: String) -> SymbolPredicate {
    Box::new(move |e| *e.sym.kind == kind)
}

fn complexity_filter(min: i64) -> SymbolPredicate {
    Box::new(move |e| e.complexity >= min)
}

fn name_regex_filter(re: Regex) -> SymbolPredicate {
    Box::new(move |e| re.is_match(&e.sym.qualified_name) || re.is_match(&e.sym.short_name))
}

fn calls_to_filter(db: &Db, target_name: &str) -> Result<SymbolPredicate> {
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
    Ok(Box::new(move |e| ids.contains(&e.sym.id)))
}

// --- Main entry point ---

pub fn handle(db: &Db, workspace_root: &Path, filter: &WinnowFilter) -> Result<serde_json::Value> {
    let limit = filter.limit.unwrap_or(20) as usize;
    let files = db.all_files()?;

    let churn_map = if filter.min_churn.is_some() || filter.rank_by.as_deref() == Some("churn") {
        let window = filter.churn_window_days.unwrap_or(90);
        crate::git::git_churn(workspace_root, window).unwrap_or_default()
    } else {
        Default::default()
    };

    // Build file-level predicates
    let mut file_filters: Vec<FilePredicate> = Vec::new();
    if let Some(ref pattern) = filter.file_glob {
        file_filters.push(glob_filter(pattern));
    }
    if let Some(min) = filter.min_churn {
        file_filters.push(churn_filter(churn_map.clone(), min));
    }

    // Build symbol-level predicates
    let mut sym_filters: Vec<SymbolPredicate> = Vec::new();
    if let Some(ref k) = filter.kind {
        sym_filters.push(kind_filter(k.clone()));
    }
    if let Some(min) = filter.min_complexity {
        sym_filters.push(complexity_filter(min));
    }
    if let Some(ref pattern) = filter.name_regex
        && let Ok(re) = Regex::new(pattern)
    {
        sym_filters.push(name_regex_filter(re));
    }
    if let Some(ref target_name) = filter.calls_to {
        sym_filters.push(calls_to_filter(db, target_name)?);
    }

    // Iterate files → symbols, applying predicates
    let mut entries: Vec<SymbolEntry> = Vec::new();

    for file in &files {
        if file_filters.iter().any(|f| !f(file)) {
            continue;
        }

        let file_churn = churn_map.get(&*file.path).copied().unwrap_or(0);
        let symbols = db.find_symbols_by_file(file.id)?;

        for sym in symbols {
            let entry = SymbolEntry {
                importance: sym.pagerank.unwrap_or(0.0),
                complexity: sym.cognitive.unwrap_or(0),
                churn: file_churn,
                file_path: file.path.to_string(),
                file_last_parsed: file.last_parsed.clone(),
                sym,
            };

            if sym_filters.iter().any(|f| !f(&entry)) {
                continue;
            }

            entries.push(entry);
        }
    }

    // Rank
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

    // Format output
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
