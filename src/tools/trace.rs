use std::collections::HashSet;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, SymbolRow};
use crate::parser::dart::DART_LIFECYCLE_METHODS;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceArgs {
    pub workspace: String,
    pub symbol: String,
    /// "forward" (entry points → symbol, default) or "backward" (symbol → leaves)
    #[serde(default)]
    pub direction: Option<String>,
    /// Max number of paths to return (default 10)
    #[serde(default)]
    pub limit: Option<usize>,
}
use crate::error::{Result, SutraError};

const DEFAULT_LIMIT: usize = 10;
const MAX_DEPTH: usize = 15;

pub fn is_known_entry_point(short_name: &str, kind: &str) -> bool {
    match short_name {
        "main" => true,
        name if kind == "method" => DART_LIFECYCLE_METHODS.contains(&name),
        _ => false,
    }
}

pub fn handle(
    db: &Db,
    symbol: &str,
    direction: Option<&str>,
    limit: Option<usize>,
) -> Result<serde_json::Value> {
    let sym = db
        .resolve_symbol(symbol, None)?
        .ok_or_else(|| SutraError::NotFound {
            tool: "sutra_trace",
            kind: format!("symbol `{symbol}`"),
            next_action: "Use sutra_find to look up the symbol name first.".into(),
        })?;

    let direction = direction.unwrap_or("forward");
    let limit = limit.unwrap_or(DEFAULT_LIMIT);

    let paths = if direction == "backward" {
        trace_backward(db, &sym, limit)?
    } else {
        trace_forward(db, &sym, limit)?
    };

    let truncated = paths.len() >= limit;

    Ok(json!({
        "symbol": sym.qualified_name,
        "direction": direction,
        "paths": paths,
        "path_count": paths.len(),
        "limit": limit,
        "truncated": truncated,
        "entry_point_rules": entry_point_rules_doc(),
    }))
}

fn trace_forward(db: &Db, target: &SymbolRow, limit: usize) -> Result<Vec<serde_json::Value>> {
    let mut paths: Vec<Vec<(i64, String)>> = Vec::new();
    let mut cycles: Vec<(Vec<(i64, String)>, String)> = Vec::new();
    let mut visited = HashSet::new();
    visited.insert(target.id);

    let mut stack: Vec<(i64, String)> = vec![(target.id, target.qualified_name.clone())];

    dfs_callers(
        db,
        &mut stack,
        &mut visited,
        &mut paths,
        &mut cycles,
        limit,
        0,
    )?;

    let mut result: Vec<serde_json::Value> = Vec::new();

    for path in paths.into_iter().take(limit) {
        let chain: Vec<&str> = path.iter().rev().map(|(_, name)| name.as_str()).collect();
        result.push(json!({
            "chain": chain,
            "has_cycle": false,
            "reaches_entry_point": true,
        }));
    }

    let remaining = limit.saturating_sub(result.len());
    for (path, cycle_to) in cycles.into_iter().take(remaining) {
        let mut chain: Vec<&str> = path.iter().rev().map(|(_, name)| name.as_str()).collect();
        chain.insert(0, &cycle_to);
        result.push(json!({
            "chain": chain,
            "has_cycle": true,
            "cycle_at": cycle_to,
            "reaches_entry_point": false,
        }));
    }

    Ok(result)
}

fn dfs_callers(
    db: &Db,
    stack: &mut Vec<(i64, String)>,
    visited: &mut HashSet<i64>,
    paths: &mut Vec<Vec<(i64, String)>>,
    cycles: &mut Vec<(Vec<(i64, String)>, String)>,
    limit: usize,
    depth: usize,
) -> Result<()> {
    if paths.len() + cycles.len() >= limit || depth >= MAX_DEPTH {
        return Ok(());
    }

    let (current_id, _) = stack.last().unwrap().clone();

    let current_sym = db.symbol_by_id(current_id)?;
    if let Some(ref s) = current_sym
        && is_known_entry_point(&s.short_name, &s.kind)
    {
        paths.push(stack.clone());
        return Ok(());
    }

    let refs = db.find_refs_to_symbol(current_id)?;
    let call_refs: Vec<_> = refs.iter().filter(|r| r.context_kind == "call").collect();

    if call_refs.is_empty() {
        // No callers — this is a structural entry point
        paths.push(stack.clone());
        return Ok(());
    }

    let mut found_any = false;
    for r in &call_refs {
        if paths.len() + cycles.len() >= limit {
            break;
        }
        if let Some(caller_sym) = db.find_enclosing_symbol(r.file_id, r.line)? {
            if !visited.insert(caller_sym.id) {
                cycles.push((stack.clone(), caller_sym.qualified_name.clone()));
                continue;
            }
            found_any = true;
            stack.push((caller_sym.id, caller_sym.qualified_name.clone()));
            dfs_callers(db, stack, visited, paths, cycles, limit, depth + 1)?;
            stack.pop();
            visited.remove(&caller_sym.id);
        }
    }

    if !found_any && cycles.is_empty() {
        paths.push(stack.clone());
    }

    Ok(())
}

fn trace_backward(db: &Db, target: &SymbolRow, limit: usize) -> Result<Vec<serde_json::Value>> {
    let mut paths: Vec<Vec<(i64, String)>> = Vec::new();
    let mut cycles: Vec<(Vec<(i64, String)>, String)> = Vec::new();
    let mut visited = HashSet::new();
    visited.insert(target.id);

    let mut stack = vec![(target.id, target.qualified_name.clone())];

    dfs_callees(
        db,
        &mut stack,
        &mut visited,
        &mut paths,
        &mut cycles,
        limit,
        0,
    )?;

    let mut result: Vec<serde_json::Value> = Vec::new();

    for path in paths.into_iter().take(limit) {
        let chain: Vec<&str> = path.iter().map(|(_, name)| name.as_str()).collect();
        result.push(json!({
            "chain": chain,
            "has_cycle": false,
            "is_leaf": true,
        }));
    }

    let remaining = limit.saturating_sub(result.len());
    for (path, cycle_to) in cycles.into_iter().take(remaining) {
        let mut chain: Vec<&str> = path.iter().map(|(_, name)| name.as_str()).collect();
        chain.push(&cycle_to);
        result.push(json!({
            "chain": chain,
            "has_cycle": true,
            "cycle_at": cycle_to,
            "is_leaf": false,
        }));
    }

    Ok(result)
}

fn dfs_callees(
    db: &Db,
    stack: &mut Vec<(i64, String)>,
    visited: &mut HashSet<i64>,
    paths: &mut Vec<Vec<(i64, String)>>,
    cycles: &mut Vec<(Vec<(i64, String)>, String)>,
    limit: usize,
    depth: usize,
) -> Result<()> {
    if paths.len() + cycles.len() >= limit || depth >= MAX_DEPTH {
        return Ok(());
    }

    let (current_id, _) = stack.last().unwrap().clone();
    let current_sym = match db.symbol_by_id(current_id)? {
        Some(s) => s,
        None => {
            paths.push(stack.clone());
            return Ok(());
        }
    };

    let refs = db.find_refs_in_file(current_sym.file_id)?;
    let callees: Vec<_> = refs
        .iter()
        .filter(|r| {
            r.context_kind == "call"
                && r.line >= current_sym.start_line
                && r.line <= current_sym.end_line
                && r.target_symbol_id.is_some()
        })
        .collect();

    if callees.is_empty() {
        paths.push(stack.clone());
        return Ok(());
    }

    for r in &callees {
        if paths.len() + cycles.len() >= limit {
            break;
        }
        let target_id = r.target_symbol_id.unwrap();
        if let Some(callee_sym) = db.symbol_by_id(target_id)? {
            if !visited.insert(callee_sym.id) {
                cycles.push((stack.clone(), callee_sym.qualified_name.clone()));
                continue;
            }
            stack.push((callee_sym.id, callee_sym.qualified_name.clone()));
            dfs_callees(db, stack, visited, paths, cycles, limit, depth + 1)?;
            stack.pop();
            visited.remove(&callee_sym.id);
        }
    }

    Ok(())
}

fn entry_point_rules_doc() -> serde_json::Value {
    json!({
        "name_based": {
            "rust": ["main"],
            "dart": std::iter::once("main")
                .chain(DART_LIFECYCLE_METHODS.iter().copied())
                .collect::<Vec<_>>(),
        },
        "structural": "any symbol with zero inbound call references is treated as an entry point",
        "default_limit": DEFAULT_LIMIT,
        "max_depth": MAX_DEPTH,
    })
}
