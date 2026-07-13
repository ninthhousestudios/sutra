use std::collections::HashSet;

use crate::db::SymbolEntry;
use crate::parser::{ExtractedImport, ExtractedRef, ExtractedSymbol, RefContextKind};

#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub original: ExtractedRef,
    pub target_symbol_id: Option<i64>,
    pub unresolved_name: Option<String>,
    /// True when resolution was skipped (Import context).
    pub skipped: bool,
}

pub fn resolve_refs(
    file_symbols: &[ExtractedSymbol],
    refs: &[ExtractedRef],
    all_symbols: &[SymbolEntry],
    file_imports: &[ExtractedImport],
    file_id: i64,
) -> Vec<ResolvedRef> {
    refs.iter()
        .map(|r| resolve_single(r, file_symbols, all_symbols, file_imports, file_id))
        .collect()
}

/// Returns true if a symbol kind is compatible with the ref's context_kind.
fn kind_compatible(context: &RefContextKind, symbol_kind: &str) -> bool {
    match context {
        RefContextKind::TypeUse | RefContextKind::Construction => matches!(
            symbol_kind,
            "struct" | "enum" | "trait" | "type_alias" | "class" | "mixin" | "extension"
        ),
        RefContextKind::Call => matches!(symbol_kind, "function" | "method" | "macro"),
        RefContextKind::FieldAccess => matches!(symbol_kind, "field" | "method"),
        // PatternBind, Other — can't narrow, accept any
        _ => true,
    }
}

fn resolve_single(
    r: &ExtractedRef,
    file_symbols: &[ExtractedSymbol],
    all_symbols: &[SymbolEntry],
    file_imports: &[ExtractedImport],
    file_id: i64,
) -> ResolvedRef {
    // Import refs are the `use`/`import` statement itself — not a usage.
    if matches!(r.context_kind, RefContextKind::Import) {
        return ResolvedRef {
            original: r.clone(),
            target_symbol_id: None,
            unresolved_name: Some(r.name.clone()),
            skipped: true,
        };
    }

    let name = &r.name;
    let use_kind_filter = matches!(
        r.context_kind,
        RefContextKind::TypeUse
            | RefContextKind::Call
            | RefContextKind::Construction
            | RefContextKind::FieldAccess
    );

    // --- Step 1: local scope (file_symbols by short_name) ---
    let local_matches: Vec<&ExtractedSymbol> = file_symbols
        .iter()
        .filter(|s| {
            s.short_name == *name
                && (!use_kind_filter || kind_compatible(&r.context_kind, s.kind.as_str()))
        })
        .collect();

    if let Some(best) = pick_nearest_local(&local_matches, r.line, file_symbols) {
        if let Some(s) = all_symbols
            .iter()
            .find(|s| s.file_id == file_id && s.qualified_name == best.qualified_name)
        {
            return resolved(r, s.id);
        }
        if let Some(s) = all_symbols
            .iter()
            .find(|s| s.file_id == file_id && s.short_name == best.short_name)
        {
            return resolved(r, s.id);
        }
    }

    // --- Step 2: import-filtered match ---
    let mut visited: HashSet<&str> = HashSet::new();
    if let Some(id) = find_via_imports(
        name,
        &r.context_kind,
        use_kind_filter,
        all_symbols,
        file_imports,
        &mut visited,
    ) {
        return resolved(r, id);
    }

    // --- Step 3: global match (all_symbols by short_name) ---
    let global_matches: Vec<&SymbolEntry> = all_symbols
        .iter()
        .filter(|s| {
            s.short_name == *name && (!use_kind_filter || kind_compatible(&r.context_kind, &s.kind))
        })
        .collect();

    if global_matches.len() == 1 {
        return resolved(r, global_matches[0].id);
    }

    if global_matches.len() > 1 {
        // Prefer same-file candidates before falling back to shortest qn.
        let same_file: Vec<&&SymbolEntry> = global_matches
            .iter()
            .filter(|s| s.file_id == file_id)
            .collect();
        if same_file.len() == 1 {
            return resolved(r, same_file[0].id);
        }

        let pool = if same_file.len() > 1 {
            same_file.into_iter().copied().collect::<Vec<_>>()
        } else {
            global_matches.clone()
        };
        let best = pool.iter().min_by_key(|s| s.qualified_name.len()).unwrap();
        return resolved(r, best.id);
    }

    // If kind filter produced no matches, retry without it — better to resolve
    // imprecisely than leave unresolved.
    if use_kind_filter {
        let fallback: Vec<&SymbolEntry> = all_symbols
            .iter()
            .filter(|s| s.short_name == *name)
            .collect();
        if fallback.len() == 1 {
            return resolved(r, fallback[0].id);
        }
    }

    // --- Step 4: unresolved ---
    ResolvedRef {
        original: r.clone(),
        target_symbol_id: None,
        unresolved_name: Some(name.clone()),
        skipped: false,
    }
}

fn resolved(r: &ExtractedRef, id: i64) -> ResolvedRef {
    ResolvedRef {
        original: r.clone(),
        target_symbol_id: Some(id),
        unresolved_name: None,
        skipped: false,
    }
}

/// Pick the best local candidate using enclosing-scope preference, then line
/// proximity as tiebreak. Scope preference: the candidate whose tightest
/// common enclosing scope with the ref is smallest wins — this correctly
/// handles inner fn shadowing, impl-method vs free-function, etc.
fn pick_nearest_local<'a>(
    candidates: &[&'a ExtractedSymbol],
    ref_line: usize,
    file_symbols: &[ExtractedSymbol],
) -> Option<&'a ExtractedSymbol> {
    if candidates.is_empty() {
        return None;
    }
    if candidates.len() == 1 {
        return Some(candidates[0]);
    }

    candidates
        .iter()
        .min_by(|a, b| {
            let scope_a = tightest_common_scope_size(a, ref_line, file_symbols);
            let scope_b = tightest_common_scope_size(b, ref_line, file_symbols);
            scope_a
                .cmp(&scope_b)
                .then_with(|| line_proximity_key(a, ref_line).cmp(&line_proximity_key(b, ref_line)))
        })
        .copied()
}

/// Size of the tightest scope in `file_symbols` that encloses both the
/// candidate's definition and `ref_line`. Smaller = more specific = better.
fn tightest_common_scope_size(
    candidate: &ExtractedSymbol,
    ref_line: usize,
    file_symbols: &[ExtractedSymbol],
) -> usize {
    file_symbols
        .iter()
        .filter(|s| {
            s.start_line <= ref_line
                && s.end_line >= ref_line
                && s.start_line <= candidate.start_line
                && s.end_line >= candidate.end_line
                && s.qualified_name != candidate.qualified_name
        })
        .map(|s| s.end_line - s.start_line)
        .min()
        .unwrap_or(usize::MAX)
}

/// Sort key matching the old heuristic: prefer preceding definitions (closer
/// start_line gets lower key), then fall back to absolute distance.
fn line_proximity_key(sym: &ExtractedSymbol, ref_line: usize) -> (bool, usize) {
    if sym.start_line <= ref_line {
        (false, ref_line - sym.start_line)
    } else {
        (true, sym.start_line - ref_line)
    }
}

fn find_via_imports(
    name: &str,
    context: &RefContextKind,
    use_kind_filter: bool,
    all_symbols: &[SymbolEntry],
    file_imports: &[ExtractedImport],
    visited: &mut HashSet<&str>,
) -> Option<i64> {
    for imp in file_imports {
        if visited.contains(imp.raw_path.as_str()) {
            continue;
        }

        let path = &imp.raw_path;
        let segments: Vec<&str> = path.split("::").collect();
        let last_segment = segments.last().copied().unwrap_or("");

        if last_segment != name && !segments.contains(&name) {
            continue;
        }

        // Full qualified_name match
        if let Some(s) = all_symbols.iter().find(|s| s.qualified_name == *path)
            && (!use_kind_filter || kind_compatible(context, &s.kind))
        {
            return Some(s.id);
        }

        // Prefix match
        let import_prefix = if let Some(pos) = path.rfind("::") {
            &path[..pos]
        } else {
            path.as_str()
        };

        if let Some(s) = all_symbols.iter().find(|s| {
            s.short_name == name
                && s.qualified_name.starts_with(import_prefix)
                && (!use_kind_filter || kind_compatible(context, &s.kind))
        }) {
            return Some(s.id);
        }

        // Short_name fallback from import context
        if last_segment == name
            && let Some(s) = all_symbols.iter().find(|s| {
                s.short_name == name && (!use_kind_filter || kind_compatible(context, &s.kind))
            })
        {
            return Some(s.id);
        }
    }

    None
}
