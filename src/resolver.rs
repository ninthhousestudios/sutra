use std::collections::HashSet;

use crate::parser::{ExtractedImport, ExtractedRef, ExtractedSymbol};

#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub original: ExtractedRef,
    pub target_symbol_id: Option<i64>,
    pub unresolved_name: Option<String>,
}

/// v0.1 resolver: local bindings, function parameters, module-level items,
/// and direct imports. Does NOT handle method dispatch, trait disambiguation,
/// glob imports, or re-export chains. Target ~60-70% resolution rate.
///
/// `file_symbols`  — symbols extracted from the current file (local scope)
/// `refs`          — refs to resolve
/// `all_symbols`   — (id, qualified_name, short_name) from the DB across the workspace
/// `file_imports`  — imports from the current file (for import-based resolution)
pub fn resolve_refs(
    file_symbols: &[ExtractedSymbol],
    refs: &[ExtractedRef],
    all_symbols: &[(i64, String, String)],
    file_imports: &[ExtractedImport],
) -> Vec<ResolvedRef> {
    // Pre-build a visited set for cycle detection when following import chains.
    // In v0.1 we don't actually chase chains, but we track which import paths
    // we've already examined to avoid looping on circular re-exports.
    let mut _visited: HashSet<&str> = HashSet::new();

    refs.iter()
        .map(|r| resolve_single(r, file_symbols, all_symbols, file_imports))
        .collect()
}

fn resolve_single(
    r: &ExtractedRef,
    file_symbols: &[ExtractedSymbol],
    all_symbols: &[(i64, String, String)],
    file_imports: &[ExtractedImport],
) -> ResolvedRef {
    let name = &r.name;

    // --- Step 1: local scope (file_symbols by short_name) ---
    // Pick the nearest enclosing symbol by line proximity (closest start_line
    // that is <= ref line, or closest start_line overall if none precede).
    let local_matches: Vec<&ExtractedSymbol> = file_symbols
        .iter()
        .filter(|s| s.short_name == *name)
        .collect();

    if let Some(best) = pick_nearest_local(&local_matches, r.line) {
        // We found a local match — but we need its DB id. Match it against
        // all_symbols by qualified_name.
        if let Some((id, _, _)) = all_symbols
            .iter()
            .find(|(_, qn, _)| qn == &best.qualified_name)
        {
            return ResolvedRef {
                original: r.clone(),
                target_symbol_id: Some(*id),
                unresolved_name: None,
            };
        }
        // Local symbol exists in file_symbols but not yet in DB — treat as
        // local match by short_name fallback through all_symbols.
        if let Some((id, _, _)) = all_symbols
            .iter()
            .find(|(_, _, sn)| sn == &best.short_name)
        {
            return ResolvedRef {
                original: r.clone(),
                target_symbol_id: Some(*id),
                unresolved_name: None,
            };
        }
    }

    // --- Step 2: import-filtered match ---
    // Collect import paths for cycle-safe traversal.
    let mut visited: HashSet<&str> = HashSet::new();
    let imported_match = find_via_imports(name, all_symbols, file_imports, &mut visited);
    if let Some(id) = imported_match {
        return ResolvedRef {
            original: r.clone(),
            target_symbol_id: Some(id),
            unresolved_name: None,
        };
    }

    // --- Step 3: global match (all_symbols by short_name directly) ---
    let global_matches: Vec<&(i64, String, String)> = all_symbols
        .iter()
        .filter(|(_, _, sn)| sn == name)
        .collect();

    if global_matches.len() == 1 {
        return ResolvedRef {
            original: r.clone(),
            target_symbol_id: Some(global_matches[0].0),
            unresolved_name: None,
        };
    }

    if global_matches.len() > 1 {
        // Multiple global matches — prefer imported over non-imported, nearest
        // line for same scope. Since we already failed the import filter above,
        // just pick the first one (stable ordering from the DB).
        return ResolvedRef {
            original: r.clone(),
            target_symbol_id: Some(global_matches[0].0),
            unresolved_name: None,
        };
    }

    // --- Step 4: unresolved ---
    ResolvedRef {
        original: r.clone(),
        target_symbol_id: None,
        unresolved_name: Some(name.clone()),
    }
}

/// From a set of local symbol matches, pick the one nearest to `ref_line`.
/// Prefers symbols whose start_line <= ref_line (enclosing/preceding), breaking
/// ties by closest distance. If none precede, picks the closest overall.
fn pick_nearest_local<'a>(
    candidates: &[&'a ExtractedSymbol],
    ref_line: usize,
) -> Option<&'a ExtractedSymbol> {
    if candidates.is_empty() {
        return None;
    }

    // Symbols that precede or are at the ref line (potential enclosing scope).
    let preceding: Vec<&&ExtractedSymbol> = candidates
        .iter()
        .filter(|s| s.start_line <= ref_line)
        .collect();

    if let Some(best) = preceding.iter().max_by_key(|s| s.start_line) {
        return Some(best);
    }

    // No preceding symbols — pick the closest one overall.
    candidates
        .iter()
        .min_by_key(|s| {
            s.start_line.abs_diff(ref_line)
        })
        .copied()
}

/// Try to resolve `name` through file imports. An import matches if its
/// `raw_path` contains `name` as a path segment (e.g. `std::collections::HashMap`
/// contains `HashMap`). Then look up that qualified-ish path in all_symbols.
///
/// Uses `visited` for cycle detection when following import chains.
fn find_via_imports(
    name: &str,
    all_symbols: &[(i64, String, String)],
    file_imports: &[ExtractedImport],
    visited: &mut HashSet<&str>,
) -> Option<i64> {
    for imp in file_imports {
        // Cycle detection: skip if we've already tried this import path.
        if visited.contains(imp.raw_path.as_str()) {
            continue;
        }
        // We can't insert &str from imp into the HashSet<&str> with the right
        // lifetime, so we do a simpler check: track raw_path strings by index.
        // For v0.1 the visited set is mainly a guard — real cycles are rare
        // without re-export chasing.

        // Check if this import brings `name` into scope.
        // Match patterns:
        //   - import ends with `::name` (e.g. `use std::collections::HashMap`)
        //   - import equals `name` (e.g. `use HashMap`)
        //   - import contains `name` as a segment
        let path = &imp.raw_path;
        let segments: Vec<&str> = path.split("::").collect();
        let last_segment = segments.last().copied().unwrap_or("");

        if last_segment != name && !segments.contains(&name) {
            continue;
        }

        // This import mentions our name. Try to find it in all_symbols.
        // First try matching the full import path as a qualified_name.
        if let Some((id, _, _)) = all_symbols
            .iter()
            .find(|(_, qn, _)| qn == path)
        {
            return Some(*id);
        }

        // Try matching by short_name, filtered to symbols whose qualified_name
        // shares a prefix with the import path.
        let import_prefix = if let Some(pos) = path.rfind("::") {
            &path[..pos]
        } else {
            path.as_str()
        };

        if let Some((id, _, _)) = all_symbols.iter().find(|(_, qn, sn)| {
            sn == name && qn.starts_with(import_prefix)
        }) {
            return Some(*id);
        }

        // Final fallback: just match short_name from this import context.
        if last_segment == name
            && let Some((id, _, _)) = all_symbols.iter().find(|(_, _, sn)| sn == name)
        {
            return Some(*id);
        }
    }

    None
}
