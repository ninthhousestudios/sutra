use std::collections::HashSet;

use crate::parser::{ExtractedImport, ExtractedRef, ExtractedSymbol, RefContextKind};

#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub original: ExtractedRef,
    pub target_symbol_id: Option<i64>,
    pub unresolved_name: Option<String>,
    /// True when resolution was skipped (Import/FieldAccess context).
    pub skipped: bool,
}

/// `all_symbols` — (id, qualified_name, short_name, kind) from the DB
pub fn resolve_refs(
    file_symbols: &[ExtractedSymbol],
    refs: &[ExtractedRef],
    all_symbols: &[(i64, String, String, String)],
    file_imports: &[ExtractedImport],
) -> Vec<ResolvedRef> {
    refs.iter()
        .map(|r| resolve_single(r, file_symbols, all_symbols, file_imports))
        .collect()
}

/// Returns true if a symbol kind is compatible with the ref's context_kind.
fn kind_compatible(context: &RefContextKind, symbol_kind: &str) -> bool {
    match context {
        RefContextKind::TypeUse => matches!(
            symbol_kind,
            "struct" | "enum" | "trait" | "type_alias" | "class" | "mixin" | "extension"
        ),
        RefContextKind::Call => matches!(symbol_kind, "function" | "method" | "macro"),
        // PatternBind, Other — can't narrow, accept any
        _ => true,
    }
}

fn resolve_single(
    r: &ExtractedRef,
    file_symbols: &[ExtractedSymbol],
    all_symbols: &[(i64, String, String, String)],
    file_imports: &[ExtractedImport],
) -> ResolvedRef {
    // Import refs are the `use`/`import` statement itself — not a usage.
    // FieldAccess requires type info we don't have.
    if matches!(
        r.context_kind,
        RefContextKind::Import | RefContextKind::FieldAccess
    ) {
        return ResolvedRef {
            original: r.clone(),
            target_symbol_id: None,
            unresolved_name: None,
            skipped: true,
        };
    }

    let name = &r.name;
    let use_kind_filter = matches!(
        r.context_kind,
        RefContextKind::TypeUse | RefContextKind::Call
    );

    // --- Step 1: local scope (file_symbols by short_name) ---
    let local_matches: Vec<&ExtractedSymbol> = file_symbols
        .iter()
        .filter(|s| {
            s.short_name == *name
                && (!use_kind_filter || kind_compatible(&r.context_kind, s.kind.as_str()))
        })
        .collect();

    if let Some(best) = pick_nearest_local(&local_matches, r.line) {
        if let Some((id, _, _, _)) = all_symbols
            .iter()
            .find(|(_, qn, _, _)| qn == &best.qualified_name)
        {
            return resolved(r, *id);
        }
        if let Some((id, _, _, _)) = all_symbols
            .iter()
            .find(|(_, _, sn, _)| sn == &best.short_name)
        {
            return resolved(r, *id);
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
    let global_matches: Vec<&(i64, String, String, String)> = all_symbols
        .iter()
        .filter(|(_, _, sn, kind)| {
            sn == name && (!use_kind_filter || kind_compatible(&r.context_kind, kind))
        })
        .collect();

    if global_matches.len() == 1 {
        return resolved(r, global_matches[0].0);
    }

    if global_matches.len() > 1 {
        let best = global_matches
            .iter()
            .min_by_key(|(_, qn, _, _)| qn.len())
            .unwrap();
        return resolved(r, best.0);
    }

    // If kind filter produced no matches, retry without it — better to resolve
    // imprecisely than leave unresolved.
    if use_kind_filter {
        let fallback: Vec<&(i64, String, String, String)> = all_symbols
            .iter()
            .filter(|(_, _, sn, _)| sn == name)
            .collect();
        if fallback.len() == 1 {
            return resolved(r, fallback[0].0);
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

fn pick_nearest_local<'a>(
    candidates: &[&'a ExtractedSymbol],
    ref_line: usize,
) -> Option<&'a ExtractedSymbol> {
    if candidates.is_empty() {
        return None;
    }

    let preceding: Vec<&&ExtractedSymbol> = candidates
        .iter()
        .filter(|s| s.start_line <= ref_line)
        .collect();

    if let Some(best) = preceding.iter().max_by_key(|s| s.start_line) {
        return Some(best);
    }

    candidates
        .iter()
        .min_by_key(|s| s.start_line.abs_diff(ref_line))
        .copied()
}

fn find_via_imports(
    name: &str,
    context: &RefContextKind,
    use_kind_filter: bool,
    all_symbols: &[(i64, String, String, String)],
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
        if let Some((id, _, _, kind)) = all_symbols.iter().find(|(_, qn, _, _)| qn == path)
            && (!use_kind_filter || kind_compatible(context, kind))
        {
            return Some(*id);
        }

        // Prefix match
        let import_prefix = if let Some(pos) = path.rfind("::") {
            &path[..pos]
        } else {
            path.as_str()
        };

        if let Some((id, _, _, _)) = all_symbols.iter().find(|(_, qn, sn, kind)| {
            sn == name
                && qn.starts_with(import_prefix)
                && (!use_kind_filter || kind_compatible(context, kind))
        }) {
            return Some(*id);
        }

        // Short_name fallback from import context
        if last_segment == name
            && let Some((id, _, _, _)) = all_symbols.iter().find(|(_, _, sn, kind)| {
                sn == name && (!use_kind_filter || kind_compatible(context, kind))
            })
        {
            return Some(*id);
        }
    }

    None
}
