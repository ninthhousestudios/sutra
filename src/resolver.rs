use std::collections::{HashMap, HashSet};

use crate::db::SymbolEntry;
use crate::parser::dart::TYPE_TRACKING_PREFIX;
use crate::parser::rust::LOCAL_BINDING_SENTINEL;
use crate::parser::{ExtractedImport, ExtractedRef, ExtractedSymbol, RefContextKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionMethod {
    ScopeChain,
    LocalBinding,
    Import,
    GlobalFallback,
    TypeTracking,
}

impl ResolutionMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScopeChain => "scope_chain",
            Self::LocalBinding => "local_binding",
            Self::Import => "import",
            Self::GlobalFallback => "global_fallback",
            Self::TypeTracking => "type_tracking",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub original: ExtractedRef,
    pub target_symbol_id: Option<i64>,
    pub unresolved_name: Option<String>,
    /// True when resolution was skipped (Import context).
    pub skipped: bool,
    pub resolution_method: Option<ResolutionMethod>,
}

/// Maps parent_symbol_id → list of (member_short_name, member_symbol_id).
/// Borrows short_name from `all_symbols` to avoid allocation.
type ClassMembers<'a> = HashMap<i64, Vec<(&'a str, i64)>>;

fn build_class_members(all_symbols: &[SymbolEntry]) -> ClassMembers<'_> {
    let mut map: ClassMembers<'_> = HashMap::new();
    for s in all_symbols {
        if let Some(parent_id) = s.parent_symbol_id {
            map.entry(parent_id)
                .or_default()
                .push((s.short_name.as_str(), s.id));
        }
    }
    map
}

pub fn resolve_refs(
    file_symbols: &[ExtractedSymbol],
    refs: &[ExtractedRef],
    all_symbols: &[SymbolEntry],
    file_imports: &[ExtractedImport],
    file_id: i64,
) -> Vec<ResolvedRef> {
    let class_members = build_class_members(all_symbols);
    refs.iter()
        .map(|r| {
            resolve_single(
                r,
                file_symbols,
                all_symbols,
                file_imports,
                file_id,
                &class_members,
            )
        })
        .collect()
}

fn kind_compatible(context: &RefContextKind, symbol_kind: &str) -> bool {
    match context {
        RefContextKind::TypeUse | RefContextKind::Construction => matches!(
            symbol_kind,
            "struct" | "enum" | "trait" | "type_alias" | "class" | "mixin" | "extension"
        ),
        RefContextKind::Call => matches!(symbol_kind, "function" | "method" | "macro"),
        RefContextKind::FieldAccess => matches!(symbol_kind, "field" | "method"),
        _ => true,
    }
}

fn resolve_single(
    r: &ExtractedRef,
    file_symbols: &[ExtractedSymbol],
    all_symbols: &[SymbolEntry],
    file_imports: &[ExtractedImport],
    file_id: i64,
    class_members: &ClassMembers<'_>,
) -> ResolvedRef {
    if matches!(r.context_kind, RefContextKind::Import) {
        return ResolvedRef {
            original: r.clone(),
            target_symbol_id: None,
            unresolved_name: Some(r.name.clone()),
            skipped: true,
            resolution_method: None,
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

    // --- Step 0: scope-chain / type-tracking hints from parse-time resolution ---
    if let Some(hint) = &r.resolved_local_target {
        if hint == LOCAL_BINDING_SENTINEL {
            return ResolvedRef {
                original: r.clone(),
                target_symbol_id: None,
                unresolved_name: Some(name.clone()),
                skipped: false,
                resolution_method: Some(ResolutionMethod::LocalBinding),
            };
        }

        // Type-tracking hint: "::type_tracking::ClassName" — look up ClassName.{name}
        // in the class_members map built from parent_symbol_id chains.
        if let Some(class_name) = hint.strip_prefix(TYPE_TRACKING_PREFIX) {
            let class_syms: Vec<&SymbolEntry> = all_symbols
                .iter()
                .filter(|s| {
                    s.short_name == class_name
                        && matches!(s.kind.as_str(), "class" | "mixin" | "extension" | "struct")
                })
                .collect();
            for class_sym in class_syms {
                if let Some(members) = class_members.get(&class_sym.id) {
                    if let Some(&(_, member_id)) = members.iter().find(|(n, _)| *n == name.as_str())
                    {
                        return resolved(r, member_id, ResolutionMethod::TypeTracking);
                    }
                }
            }
            // Type found but member not in DB — fall through to standard resolution.
        }

        if let Some(s) = all_symbols
            .iter()
            .find(|s| s.file_id == file_id && s.qualified_name == *hint)
        {
            return resolved(r, s.id, ResolutionMethod::ScopeChain);
        }
    }

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
            return resolved(r, s.id, ResolutionMethod::ScopeChain);
        }
        if let Some(s) = all_symbols
            .iter()
            .find(|s| s.file_id == file_id && s.short_name == best.short_name)
        {
            return resolved(r, s.id, ResolutionMethod::ScopeChain);
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
        return resolved(r, id, ResolutionMethod::Import);
    }

    // --- Step 3: global match (all_symbols by short_name) ---
    let global_matches: Vec<&SymbolEntry> = all_symbols
        .iter()
        .filter(|s| {
            s.short_name == *name && (!use_kind_filter || kind_compatible(&r.context_kind, &s.kind))
        })
        .collect();

    if global_matches.len() == 1 {
        return resolved(r, global_matches[0].id, ResolutionMethod::GlobalFallback);
    }

    if global_matches.len() > 1 {
        let same_file: Vec<&&SymbolEntry> = global_matches
            .iter()
            .filter(|s| s.file_id == file_id)
            .collect();
        if same_file.len() == 1 {
            return resolved(r, same_file[0].id, ResolutionMethod::GlobalFallback);
        }

        let pool = if same_file.len() > 1 {
            same_file.into_iter().copied().collect::<Vec<_>>()
        } else {
            global_matches.clone()
        };
        let best = pool.iter().min_by_key(|s| s.qualified_name.len()).unwrap();
        return resolved(r, best.id, ResolutionMethod::GlobalFallback);
    }

    if use_kind_filter {
        let fallback: Vec<&SymbolEntry> = all_symbols
            .iter()
            .filter(|s| s.short_name == *name)
            .collect();
        if fallback.len() == 1 {
            return resolved(r, fallback[0].id, ResolutionMethod::GlobalFallback);
        }
    }

    ResolvedRef {
        original: r.clone(),
        target_symbol_id: None,
        unresolved_name: Some(name.clone()),
        skipped: false,
        resolution_method: None,
    }
}

fn resolved(r: &ExtractedRef, id: i64, method: ResolutionMethod) -> ResolvedRef {
    ResolvedRef {
        original: r.clone(),
        target_symbol_id: Some(id),
        unresolved_name: None,
        skipped: false,
        resolution_method: Some(method),
    }
}

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

        if let Some(s) = all_symbols.iter().find(|s| s.qualified_name == *path)
            && (!use_kind_filter || kind_compatible(context, &s.kind))
        {
            return Some(s.id);
        }

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
