use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::error::Result;
use crate::git::{self, DiffFileEntry};
use crate::parser::{
    self, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, flatten_symbols,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Deleted,
    SignatureChanged,
    BodyChanged,
    CosmeticChanged,
    Renamed,
    Moved,
}

#[derive(Debug, Clone, Serialize)]
pub struct CalleeDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolChange {
    pub symbol: String,
    pub kind: String,
    pub change: ChangeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee_diff: Option<CalleeDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_file: Option<String>,
}

pub struct UnmatchedSymbol {
    pub qualified_name: String,
    pub short_name: String,
    pub kind: String,
    pub file: String,
    pub body_hash: String,
    pub structural_hash: Option<String>,
    pub start_line: usize,
    pub content: String,
}

pub struct ClassifyResult {
    pub changes: Vec<SymbolChange>,
    pub unmatched_old: Vec<UnmatchedSymbol>,
    pub unmatched_new: Vec<UnmatchedSymbol>,
}

pub struct ResolveResult {
    pub changes: Vec<(String, SymbolChange)>,
    pub matched_old: HashSet<usize>,
    pub matched_new: HashSet<usize>,
}

fn extract_content(source: &str, sym: &ExtractedSymbol) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let start = sym.start_line.saturating_sub(1);
    let end = sym.end_line.min(lines.len());
    lines[start..end].join("\n")
}

fn body_hash(source: &str, sym: &ExtractedSymbol) -> String {
    let content = extract_content(source, sym);
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

fn callees_in_range(refs: &[ExtractedRef], start_line: usize, end_line: usize) -> BTreeSet<String> {
    refs.iter()
        .filter(|r| {
            r.context_kind == RefContextKind::Call && r.line >= start_line && r.line <= end_line
        })
        .map(|r| r.name.clone())
        .collect()
}

fn callee_diff(
    old_refs: &[ExtractedRef],
    new_refs: &[ExtractedRef],
    old_sym: &ExtractedSymbol,
    new_sym: &ExtractedSymbol,
) -> CalleeDiff {
    let old_callees = callees_in_range(old_refs, old_sym.start_line, old_sym.end_line);
    let new_callees = callees_in_range(new_refs, new_sym.start_line, new_sym.end_line);
    CalleeDiff {
        added: new_callees.difference(&old_callees).cloned().collect(),
        removed: old_callees.difference(&new_callees).cloned().collect(),
    }
}

fn parent_qualified<'a>(qualified_name: &'a str, short_name: &str) -> Option<&'a str> {
    qualified_name
        .strip_suffix(short_name)
        .and_then(|prefix| prefix.strip_suffix("::"))
}

fn build_unmatched(parse: &ParseResult, source: &str, file: &str) -> Vec<UnmatchedSymbol> {
    let flat = flatten_symbols(&parse.symbols);
    flat.iter()
        .map(|sym| {
            let content = extract_content(source, sym);
            let bh = blake3::hash(content.as_bytes()).to_hex().to_string();
            UnmatchedSymbol {
                qualified_name: sym.qualified_name.to_string(),
                short_name: sym.short_name.to_string(),
                kind: sym.kind.as_str().to_string(),
                file: file.to_string(),
                body_hash: bh,
                structural_hash: sym.structural_hash.as_ref().map(|s| s.to_string()),
                start_line: sym.start_line,
                content,
            }
        })
        .collect()
}

pub fn classify_symbols(
    old_parse: &ParseResult,
    new_parse: &ParseResult,
    old_source: &str,
    new_source: &str,
    old_file: &str,
    new_file: &str,
) -> ClassifyResult {
    let old_flat = flatten_symbols(&old_parse.symbols);
    let new_flat = flatten_symbols(&new_parse.symbols);

    type SymKey<'a> = (&'a str, &'a str);
    fn sym_key(s: &ExtractedSymbol) -> SymKey<'_> {
        (s.qualified_name.as_str(), s.kind.as_str())
    }

    let old_map: HashMap<SymKey<'_>, &ExtractedSymbol> =
        old_flat.iter().map(|s| (sym_key(s), *s)).collect();
    let new_map: HashMap<SymKey<'_>, &ExtractedSymbol> =
        new_flat.iter().map(|s| (sym_key(s), *s)).collect();

    let mut changes = Vec::new();
    let mut unmatched_new = Vec::new();

    for new_sym in &new_flat {
        match old_map.get(&sym_key(new_sym)) {
            None => {
                let content = extract_content(new_source, new_sym);
                let bh = blake3::hash(content.as_bytes()).to_hex().to_string();
                unmatched_new.push(UnmatchedSymbol {
                    qualified_name: new_sym.qualified_name.to_string(),
                    short_name: new_sym.short_name.to_string(),
                    kind: new_sym.kind.as_str().to_string(),
                    file: new_file.to_string(),
                    body_hash: bh,
                    structural_hash: new_sym.structural_hash.as_ref().map(|s| s.to_string()),
                    start_line: new_sym.start_line,
                    content,
                });
            }
            Some(old_sym) => {
                let sig_changed = match (&old_sym.signature_hash, &new_sym.signature_hash) {
                    (Some(oh), Some(nh)) => oh != nh,
                    (None, Some(_)) | (Some(_), None) => true,
                    (None, None) => false,
                };
                if sig_changed {
                    changes.push(SymbolChange {
                        symbol: new_sym.qualified_name.to_string(),
                        kind: new_sym.kind.as_str().to_string(),
                        change: ChangeKind::SignatureChanged,
                        callee_diff: None,
                        from_symbol: None,
                        from_file: None,
                    });
                } else {
                    let old_hash = body_hash(old_source, old_sym);
                    let new_hash = body_hash(new_source, new_sym);
                    if old_hash != new_hash {
                        let is_cosmetic = match (&old_sym.structural_hash, &new_sym.structural_hash)
                        {
                            (Some(oh), Some(nh)) => oh == nh,
                            _ => false,
                        };
                        if is_cosmetic {
                            changes.push(SymbolChange {
                                symbol: new_sym.qualified_name.to_string(),
                                kind: new_sym.kind.as_str().to_string(),
                                change: ChangeKind::CosmeticChanged,
                                callee_diff: None,
                                from_symbol: None,
                                from_file: None,
                            });
                        } else {
                            let cd = callee_diff(
                                &old_parse.references,
                                &new_parse.references,
                                old_sym,
                                new_sym,
                            );
                            let cd = if cd.added.is_empty() && cd.removed.is_empty() {
                                None
                            } else {
                                Some(cd)
                            };
                            changes.push(SymbolChange {
                                symbol: new_sym.qualified_name.to_string(),
                                kind: new_sym.kind.as_str().to_string(),
                                change: ChangeKind::BodyChanged,
                                callee_diff: cd,
                                from_symbol: None,
                                from_file: None,
                            });
                        }
                    }
                }
            }
        }
    }

    let mut unmatched_old = Vec::new();
    for old_sym in &old_flat {
        if !new_map.contains_key(&sym_key(old_sym)) {
            let content = extract_content(old_source, old_sym);
            let bh = blake3::hash(content.as_bytes()).to_hex().to_string();
            unmatched_old.push(UnmatchedSymbol {
                qualified_name: old_sym.qualified_name.to_string(),
                short_name: old_sym.short_name.to_string(),
                kind: old_sym.kind.as_str().to_string(),
                file: old_file.to_string(),
                body_hash: bh,
                structural_hash: old_sym.structural_hash.as_ref().map(|s| s.to_string()),
                start_line: old_sym.start_line,
                content,
            });
        }
    }

    ClassifyResult {
        changes,
        unmatched_old,
        unmatched_new,
    }
}

const SAME_FILE_MIN_SIMILARITY: f64 = 0.3;
const MIN_COUNT_RATIO: f64 = 0.6;

fn jaccard_similarity(a_content: &str, b_content: &str) -> f64 {
    let mut a_total = 0usize;
    let mut a_unique: HashSet<&str> = HashSet::new();
    for tok in a_content.split_whitespace() {
        a_total += 1;
        a_unique.insert(tok);
    }

    let mut b_total = 0usize;
    let mut b_unique: HashSet<&str> = HashSet::new();
    for tok in b_content.split_whitespace() {
        b_total += 1;
        b_unique.insert(tok);
    }

    let (min_c, max_c) = if a_total < b_total {
        (a_total, b_total)
    } else {
        (b_total, a_total)
    };

    if max_c > 0 && (min_c as f64 / max_c as f64) < MIN_COUNT_RATIO {
        return 0.0;
    }

    let intersection = a_unique.intersection(&b_unique).count();
    let union = a_unique.len() + b_unique.len() - intersection;
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
}

pub fn resolve_renames(
    unmatched_old: &[UnmatchedSymbol],
    unmatched_new: &[UnmatchedSymbol],
) -> ResolveResult {
    let mut changes: Vec<(String, SymbolChange)> = Vec::new();
    let mut matched_old: HashSet<usize> = HashSet::new();
    let mut matched_new: HashSet<usize> = HashSet::new();

    // Phase 2: hash match — body_hash first, structural_hash fallback
    let mut old_by_body: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut old_by_structural: HashMap<&str, Vec<usize>> = HashMap::new();
    for (idx, sym) in unmatched_old.iter().enumerate() {
        old_by_body.entry(&sym.body_hash).or_default().push(idx);
        if let Some(ref sh) = sym.structural_hash {
            old_by_structural.entry(sh.as_str()).or_default().push(idx);
        }
    }

    for (new_idx, new_sym) in unmatched_new.iter().enumerate() {
        if matched_new.contains(&new_idx) {
            continue;
        }

        let found = old_by_body
            .get_mut(new_sym.body_hash.as_str())
            .and_then(|indices| {
                indices
                    .iter()
                    .position(|&i| !matched_old.contains(&i))
                    .map(|pos| indices.remove(pos))
            });

        let found = found.or_else(|| {
            new_sym.structural_hash.as_ref().and_then(|sh| {
                old_by_structural.get_mut(sh.as_str()).and_then(|indices| {
                    indices
                        .iter()
                        .position(|&i| !matched_old.contains(&i))
                        .map(|pos| indices.remove(pos))
                })
            })
        });

        if let Some(old_idx) = found {
            let old_sym = &unmatched_old[old_idx];

            // Skip if everything is identical — only a disambiguator shifted
            if old_sym.short_name == new_sym.short_name
                && old_sym.file == new_sym.file
                && old_sym.body_hash == new_sym.body_hash
                && parent_qualified(&old_sym.qualified_name, &old_sym.short_name)
                    == parent_qualified(&new_sym.qualified_name, &new_sym.short_name)
            {
                matched_old.insert(old_idx);
                matched_new.insert(new_idx);
                continue;
            }

            let (change_kind, from_symbol, from_file) = if old_sym.file != new_sym.file {
                let fs = (old_sym.qualified_name != new_sym.qualified_name)
                    .then(|| old_sym.qualified_name.to_string());
                (ChangeKind::Moved, fs, Some(old_sym.file.to_string()))
            } else if old_sym.qualified_name != new_sym.qualified_name
                || old_sym.kind != new_sym.kind
            {
                (
                    ChangeKind::Renamed,
                    Some(old_sym.qualified_name.to_string()),
                    None,
                )
            } else {
                // Same name, same file — content matched by hash so no real change
                matched_old.insert(old_idx);
                matched_new.insert(new_idx);
                continue;
            };

            matched_old.insert(old_idx);
            matched_new.insert(new_idx);

            changes.push((
                new_sym.file.to_string(),
                SymbolChange {
                    symbol: new_sym.qualified_name.to_string(),
                    kind: new_sym.kind.to_string(),
                    change: change_kind,
                    callee_diff: None,
                    from_symbol,
                    from_file,
                },
            ));
        }
    }

    // Phase 3: same-file signature match via Jaccard similarity
    type SigKey<'a> = (&'a str, &'a str, &'a str, Option<&'a str>);

    let mut old_by_sig: HashMap<SigKey<'_>, Vec<usize>> = HashMap::new();
    for (idx, sym) in unmatched_old.iter().enumerate() {
        if matched_old.contains(&idx) {
            continue;
        }
        let key: SigKey = (
            &sym.file,
            &sym.kind,
            &sym.short_name,
            parent_qualified(&sym.qualified_name, &sym.short_name),
        );
        old_by_sig.entry(key).or_default().push(idx);
    }

    let mut new_by_sig: HashMap<SigKey<'_>, Vec<usize>> = HashMap::new();
    for (idx, sym) in unmatched_new.iter().enumerate() {
        if matched_new.contains(&idx) {
            continue;
        }
        let key: SigKey = (
            &sym.file,
            &sym.kind,
            &sym.short_name,
            parent_qualified(&sym.qualified_name, &sym.short_name),
        );
        new_by_sig.entry(key).or_default().push(idx);
    }

    let common_keys: Vec<SigKey> = new_by_sig
        .keys()
        .filter(|k| old_by_sig.contains_key(k))
        .copied()
        .collect();

    for key in common_keys {
        let old_indices = &old_by_sig[&key];
        let new_indices = &new_by_sig[&key];

        let mut candidates: Vec<(f64, usize, usize, usize)> = Vec::new();

        for &new_idx in new_indices {
            if matched_new.contains(&new_idx) {
                continue;
            }
            for &old_idx in old_indices {
                if matched_old.contains(&old_idx) {
                    continue;
                }
                let score = jaccard_similarity(
                    &unmatched_old[old_idx].content,
                    &unmatched_new[new_idx].content,
                );
                let line_dist = unmatched_old[old_idx]
                    .start_line
                    .abs_diff(unmatched_new[new_idx].start_line);
                candidates.push((score, line_dist, old_idx, new_idx));
            }
        }

        candidates.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
                .then_with(|| a.3.cmp(&b.3))
        });

        for (score, _line_dist, old_idx, new_idx) in candidates {
            if !score.is_finite() || score < SAME_FILE_MIN_SIMILARITY {
                continue;
            }
            if matched_old.contains(&old_idx) || matched_new.contains(&new_idx) {
                continue;
            }

            matched_old.insert(old_idx);
            matched_new.insert(new_idx);

            let old_sym = &unmatched_old[old_idx];
            let new_sym = &unmatched_new[new_idx];

            if old_sym.body_hash == new_sym.body_hash {
                continue;
            }

            let change_kind = if old_sym.qualified_name != new_sym.qualified_name {
                ChangeKind::Renamed
            } else {
                ChangeKind::BodyChanged
            };

            let from_symbol = (old_sym.qualified_name != new_sym.qualified_name)
                .then(|| old_sym.qualified_name.to_string());

            changes.push((
                new_sym.file.to_string(),
                SymbolChange {
                    symbol: new_sym.qualified_name.to_string(),
                    kind: new_sym.kind.to_string(),
                    change: change_kind,
                    callee_diff: None,
                    from_symbol,
                    from_file: None,
                },
            ));
        }
    }

    ResolveResult {
        changes,
        matched_old,
        matched_new,
    }
}

fn collapse_unmatched(
    unmatched_old: &[UnmatchedSymbol],
    unmatched_new: &[UnmatchedSymbol],
    resolve: &ResolveResult,
) -> Vec<SymbolChange> {
    let mut out = Vec::new();

    for (idx, sym) in unmatched_new.iter().enumerate() {
        if !resolve.matched_new.contains(&idx) {
            out.push(SymbolChange {
                symbol: sym.qualified_name.to_string(),
                kind: sym.kind.to_string(),
                change: ChangeKind::Added,
                callee_diff: None,
                from_symbol: None,
                from_file: None,
            });
        }
    }

    for (idx, sym) in unmatched_old.iter().enumerate() {
        if !resolve.matched_old.contains(&idx) {
            out.push(SymbolChange {
                symbol: sym.qualified_name.to_string(),
                kind: sym.kind.to_string(),
                change: ChangeKind::Deleted,
                callee_diff: None,
                from_symbol: None,
                from_file: None,
            });
        }
    }

    out
}

fn language_for_path(path: &str) -> Option<&'static str> {
    let ext = Path::new(path).extension()?.to_str()?;
    match ext {
        "rs" => Some("rust"),
        "dart" => Some("dart"),
        _ => None,
    }
}

pub fn diff_file(
    workspace_root: &Path,
    path: &str,
    old_path: Option<&str>,
    base: &str,
    head: &str,
) -> Result<Vec<SymbolChange>> {
    let old_file = old_path.unwrap_or(path);
    let language = match language_for_path(path).or_else(|| language_for_path(old_file)) {
        Some(l) => l,
        None => return Ok(Vec::new()),
    };

    let old_source = git::git_file_content_at(workspace_root, base, old_file)?;
    let new_source = git::git_file_content_at(workspace_root, head, path)?;

    match (old_source, new_source) {
        (None, None) => Ok(Vec::new()),
        (None, Some(new_src)) => {
            let new_parse = parser::parse_file(&new_src, language, path)?;
            let flat = flatten_symbols(&new_parse.symbols);
            Ok(flat
                .iter()
                .map(|s| SymbolChange {
                    symbol: s.qualified_name.to_string(),
                    kind: s.kind.as_str().to_string(),
                    change: ChangeKind::Added,
                    callee_diff: None,
                    from_symbol: None,
                    from_file: None,
                })
                .collect())
        }
        (Some(old_src), None) => {
            let old_parse = parser::parse_file(&old_src, language, old_file)?;
            let flat = flatten_symbols(&old_parse.symbols);
            Ok(flat
                .iter()
                .map(|s| SymbolChange {
                    symbol: s.qualified_name.to_string(),
                    kind: s.kind.as_str().to_string(),
                    change: ChangeKind::Deleted,
                    callee_diff: None,
                    from_symbol: None,
                    from_file: None,
                })
                .collect())
        }
        (Some(old_src), Some(new_src)) => {
            let old_parse = parser::parse_file(&old_src, language, old_file)?;
            let new_parse = parser::parse_file(&new_src, language, path)?;
            let result = classify_symbols(&old_parse, &new_parse, &old_src, &new_src, path, path);

            let resolve = resolve_renames(&result.unmatched_old, &result.unmatched_new);
            let mut changes = result.changes;
            changes.extend(collapse_unmatched(
                &result.unmatched_old,
                &result.unmatched_new,
                &resolve,
            ));
            changes.extend(resolve.changes.into_iter().map(|(_, c)| c));

            Ok(changes)
        }
    }
}

pub struct DiffFilesResult {
    pub per_file: HashMap<String, Vec<SymbolChange>>,
    pub errors: HashMap<String, String>,
}

pub fn diff_files(
    workspace_root: &Path,
    entries: &[DiffFileEntry],
    base: &str,
    head: &str,
) -> DiffFilesResult {
    let mut all_unmatched_old: Vec<UnmatchedSymbol> = Vec::new();
    let mut all_unmatched_new: Vec<UnmatchedSymbol> = Vec::new();
    let mut per_file: HashMap<String, Vec<SymbolChange>> = HashMap::new();
    let mut errors: HashMap<String, String> = HashMap::new();

    for entry in entries {
        let old_file = entry.old_path.as_deref().unwrap_or(&entry.path);
        let new_file = &entry.path;

        let language = match language_for_path(new_file).or_else(|| language_for_path(old_file)) {
            Some(l) => l,
            None => continue,
        };

        let mut process = || -> Result<()> {
            let old_source = git::git_file_content_at(workspace_root, base, old_file)?;
            let new_source = git::git_file_content_at(workspace_root, head, new_file)?;

            match (old_source, new_source) {
                (None, None) => {}
                (None, Some(new_src)) => {
                    let new_parse = parser::parse_file(&new_src, language, new_file)?;
                    all_unmatched_new.extend(build_unmatched(&new_parse, &new_src, new_file));
                }
                (Some(old_src), None) => {
                    let old_parse = parser::parse_file(&old_src, language, old_file)?;
                    all_unmatched_old.extend(build_unmatched(&old_parse, &old_src, new_file));
                }
                (Some(old_src), Some(new_src)) => {
                    let old_parse = parser::parse_file(&old_src, language, old_file)?;
                    let new_parse = parser::parse_file(&new_src, language, new_file)?;
                    let result = classify_symbols(
                        &old_parse, &new_parse, &old_src, &new_src, new_file, new_file,
                    );
                    per_file
                        .entry(new_file.to_string())
                        .or_default()
                        .extend(result.changes);
                    all_unmatched_old.extend(result.unmatched_old);
                    all_unmatched_new.extend(result.unmatched_new);
                }
            }
            Ok(())
        };
        if let Err(e) = process() {
            errors.insert(new_file.to_string(), e.to_string());
        }
    }

    let resolve = resolve_renames(&all_unmatched_old, &all_unmatched_new);

    for (file, change) in resolve.changes {
        per_file.entry(file).or_default().push(change);
    }

    for (idx, sym) in all_unmatched_new.iter().enumerate() {
        if !resolve.matched_new.contains(&idx) {
            per_file
                .entry(sym.file.to_string())
                .or_default()
                .push(SymbolChange {
                    symbol: sym.qualified_name.to_string(),
                    kind: sym.kind.to_string(),
                    change: ChangeKind::Added,
                    callee_diff: None,
                    from_symbol: None,
                    from_file: None,
                });
        }
    }

    for (idx, sym) in all_unmatched_old.iter().enumerate() {
        if !resolve.matched_old.contains(&idx) {
            per_file
                .entry(sym.file.to_string())
                .or_default()
                .push(SymbolChange {
                    symbol: sym.qualified_name.to_string(),
                    kind: sym.kind.to_string(),
                    change: ChangeKind::Deleted,
                    callee_diff: None,
                    from_symbol: None,
                    from_file: None,
                });
        }
    }

    DiffFilesResult { per_file, errors }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ExtractedSymbol, ParseResult, SymbolKind};

    fn make_sym(
        name: &str,
        kind: SymbolKind,
        sig_hash: Option<&str>,
        start: usize,
        end: usize,
    ) -> ExtractedSymbol {
        make_sym_full(name, kind, sig_hash, None, start, end)
    }

    fn make_sym_full(
        name: &str,
        kind: SymbolKind,
        sig_hash: Option<&str>,
        structural_hash: Option<&str>,
        start: usize,
        end: usize,
    ) -> ExtractedSymbol {
        ExtractedSymbol {
            qualified_name: name.to_string(),
            short_name: name.to_string(),
            kind,
            signature: None,
            signature_hash: sig_hash.map(|s| s.to_string()),
            structural_hash: structural_hash.map(|s| s.to_string()),
            visibility: None,
            start_line: start,
            start_col: 0,
            end_line: end,
            end_col: 0,
            children: vec![],
            parent_symbol_id: None,
            docstring: None,
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        }
    }

    fn make_parse(symbols: Vec<ExtractedSymbol>, references: Vec<ExtractedRef>) -> ParseResult {
        ParseResult {
            file_path: "test.rs".to_string(),
            language: "rust".to_string(),
            symbols,
            references,
            imports: vec![],
            parsed_ok: true,
            line_count: 100,
        }
    }

    fn make_ref(name: &str, line: usize) -> ExtractedRef {
        ExtractedRef {
            name: name.to_string(),
            line,
            col: 0,
            context_kind: RefContextKind::Call,
            resolved_local_target: None,
        }
    }

    fn classify(
        old: &ParseResult,
        new: &ParseResult,
        old_src: &str,
        new_src: &str,
    ) -> Vec<SymbolChange> {
        let result = classify_symbols(old, new, old_src, new_src, "test.rs", "test.rs");
        let resolve = resolve_renames(&result.unmatched_old, &result.unmatched_new);
        let mut changes = result.changes;
        changes.extend(collapse_unmatched(
            &result.unmatched_old,
            &result.unmatched_new,
            &resolve,
        ));
        changes.extend(resolve.changes.into_iter().map(|(_, c)| c));
        changes
    }

    #[test]
    fn test_all_added() {
        let old = make_parse(vec![], vec![]);
        let new = make_parse(
            vec![make_sym("foo", SymbolKind::Function, Some("aaa"), 1, 5)],
            vec![],
        );
        let changes = classify(&old, &new, "", "fn foo() { 1 }");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].symbol, "foo");
        assert_eq!(changes[0].change, ChangeKind::Added);
    }

    #[test]
    fn test_all_deleted() {
        let old = make_parse(
            vec![make_sym("bar", SymbolKind::Function, Some("bbb"), 1, 3)],
            vec![],
        );
        let new = make_parse(vec![], vec![]);
        let changes = classify(&old, &new, "fn bar() {}", "");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].symbol, "bar");
        assert_eq!(changes[0].change, ChangeKind::Deleted);
    }

    #[test]
    fn test_signature_changed() {
        let old = make_parse(
            vec![make_sym(
                "baz",
                SymbolKind::Function,
                Some("old_hash"),
                1,
                3,
            )],
            vec![],
        );
        let new = make_parse(
            vec![make_sym(
                "baz",
                SymbolKind::Function,
                Some("new_hash"),
                1,
                3,
            )],
            vec![],
        );
        let changes = classify(&old, &new, "fn baz() {}", "fn baz(x: i32) {}");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::SignatureChanged);
    }

    #[test]
    fn test_body_changed() {
        let old = make_parse(
            vec![make_sym("calc", SymbolKind::Function, Some("same"), 1, 3)],
            vec![],
        );
        let source_old = "fn calc() {\n  1 + 1\n}";
        let source_new = "fn calc() {\n  2 + 2\n}";
        let new = make_parse(
            vec![make_sym("calc", SymbolKind::Function, Some("same"), 1, 3)],
            vec![],
        );
        let changes = classify(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
    }

    #[test]
    fn test_unchanged_omitted() {
        let source = "fn noop() {\n  42\n}";
        let old = make_parse(
            vec![make_sym("noop", SymbolKind::Function, Some("x"), 1, 3)],
            vec![],
        );
        let new = make_parse(
            vec![make_sym("noop", SymbolKind::Function, Some("x"), 1, 3)],
            vec![],
        );
        let changes = classify(&old, &new, source, source);
        assert!(changes.is_empty());
    }

    #[test]
    fn test_callee_diff() {
        let source_old = "fn run() {\n  old_call()\n  shared()\n}";
        let source_new = "fn run() {\n  new_call()\n  shared()\n}";
        let old = make_parse(
            vec![make_sym("run", SymbolKind::Function, Some("h"), 1, 3)],
            vec![make_ref("old_call", 2), make_ref("shared", 3)],
        );
        let new = make_parse(
            vec![make_sym("run", SymbolKind::Function, Some("h"), 1, 3)],
            vec![make_ref("new_call", 2), make_ref("shared", 3)],
        );
        let changes = classify(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
        let cd = changes[0].callee_diff.as_ref().unwrap();
        assert_eq!(cd.added, vec!["new_call"]);
        assert_eq!(cd.removed, vec!["old_call"]);
    }

    #[test]
    fn test_sig_none_to_some_is_changed() {
        let old = make_parse(
            vec![make_sym("thing", SymbolKind::Struct, None, 1, 3)],
            vec![],
        );
        let new = make_parse(
            vec![make_sym("thing", SymbolKind::Struct, Some("abc"), 1, 3)],
            vec![],
        );
        let changes = classify(&old, &new, "struct thing {}", "struct thing {}");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::SignatureChanged);
    }

    #[test]
    fn test_callee_diff_empty_when_calls_unchanged() {
        let source_old = "fn f() {\n  a()\n}";
        let source_new = "fn f() {\n  a()\n  let x = 1;\n}";
        let old = make_parse(
            vec![make_sym("f", SymbolKind::Function, Some("h"), 1, 2)],
            vec![make_ref("a", 2)],
        );
        let new = make_parse(
            vec![make_sym("f", SymbolKind::Function, Some("h"), 1, 3)],
            vec![make_ref("a", 2)],
        );
        let changes = classify(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
        assert!(changes[0].callee_diff.is_none());
    }

    #[test]
    fn test_struct_and_impl_not_conflated() {
        let source = "struct Foo {}\nimpl Foo {\n  fn bar() {}\n}";
        let old = make_parse(
            vec![
                make_sym("Foo", SymbolKind::Struct, None, 1, 1),
                make_sym("Foo", SymbolKind::Impl, None, 2, 4),
            ],
            vec![],
        );
        let new_source = "struct Foo { x: i32 }\nimpl Foo {\n  fn bar() {}\n}";
        let new = make_parse(
            vec![
                make_sym("Foo", SymbolKind::Struct, None, 1, 1),
                make_sym("Foo", SymbolKind::Impl, None, 2, 4),
            ],
            vec![],
        );
        let changes = classify(&old, &new, source, new_source);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, "struct");
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
    }

    #[test]
    fn test_cosmetic_change_reformat() {
        let source_old = "fn calc() {\n  1 + 1\n}";
        let source_new = "fn calc() {\n    1 + 1\n}";
        let old = make_parse(
            vec![make_sym_full(
                "calc",
                SymbolKind::Function,
                Some("same"),
                Some("structural_a"),
                1,
                3,
            )],
            vec![],
        );
        let new = make_parse(
            vec![make_sym_full(
                "calc",
                SymbolKind::Function,
                Some("same"),
                Some("structural_a"),
                1,
                3,
            )],
            vec![],
        );
        let changes = classify(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::CosmeticChanged);
        assert!(changes[0].callee_diff.is_none());
    }

    #[test]
    fn test_structural_hash_differs_is_body_changed() {
        let source_old = "fn calc() {\n  1 + 1\n}";
        let source_new = "fn calc() {\n  2 + 2\n}";
        let old = make_parse(
            vec![make_sym_full(
                "calc",
                SymbolKind::Function,
                Some("same"),
                Some("structural_a"),
                1,
                3,
            )],
            vec![],
        );
        let new = make_parse(
            vec![make_sym_full(
                "calc",
                SymbolKind::Function,
                Some("same"),
                Some("structural_b"),
                1,
                3,
            )],
            vec![],
        );
        let changes = classify(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
    }

    #[test]
    fn test_cosmetic_falls_back_when_no_structural_hash() {
        let source_old = "fn f() {\n  1\n}";
        let source_new = "fn f() {\n    1\n}";
        let old = make_parse(
            vec![make_sym("f", SymbolKind::Function, Some("h"), 1, 3)],
            vec![],
        );
        let new = make_parse(
            vec![make_sym("f", SymbolKind::Function, Some("h"), 1, 3)],
            vec![],
        );
        let changes = classify(&old, &new, source_old, source_new);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::BodyChanged);
    }

    #[test]
    fn test_rename_same_body_via_structural_hash() {
        let source = "fn foo() {\n  42\n}";
        let old = make_parse(
            vec![make_sym_full(
                "foo",
                SymbolKind::Function,
                Some("h"),
                Some("sh"),
                1,
                3,
            )],
            vec![],
        );
        let new = make_parse(
            vec![make_sym_full(
                "bar",
                SymbolKind::Function,
                Some("h2"),
                Some("sh"),
                1,
                3,
            )],
            vec![],
        );
        let changes = classify(&old, &new, source, source);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::Renamed);
        assert_eq!(changes[0].symbol, "bar");
        assert_eq!(changes[0].from_symbol.as_deref(), Some("foo"));
        assert!(changes[0].from_file.is_none());
    }

    #[test]
    fn test_cross_file_move_via_body_hash() {
        let source = "fn helper() {\n  do_work()\n}";
        let old_parse = make_parse(
            vec![make_sym_full(
                "helper",
                SymbolKind::Function,
                Some("h"),
                Some("sh"),
                1,
                3,
            )],
            vec![],
        );
        let new_parse = make_parse(
            vec![make_sym_full(
                "helper",
                SymbolKind::Function,
                Some("h"),
                Some("sh"),
                1,
                3,
            )],
            vec![],
        );

        let old_result = classify_symbols(
            &old_parse,
            &make_parse(vec![], vec![]),
            source,
            "",
            "a.rs",
            "a.rs",
        );
        let new_result = classify_symbols(
            &make_parse(vec![], vec![]),
            &new_parse,
            "",
            source,
            "b.rs",
            "b.rs",
        );

        let mut all_old = old_result.unmatched_old;
        all_old.extend(new_result.unmatched_old);
        let mut all_new = old_result.unmatched_new;
        all_new.extend(new_result.unmatched_new);

        let resolve = resolve_renames(&all_old, &all_new);
        assert_eq!(resolve.changes.len(), 1);
        let (file, change) = &resolve.changes[0];
        assert_eq!(file, "b.rs");
        assert_eq!(change.change, ChangeKind::Moved);
        assert_eq!(change.symbol, "helper");
        assert_eq!(change.from_file.as_deref(), Some("a.rs"));
    }

    #[test]
    fn test_disambiguator_shift_skipped() {
        let source = "fn foo() { 1 }";
        let old = make_parse(
            vec![{
                let mut s =
                    make_sym_full("Foo::bar#1", SymbolKind::Function, Some("h"), None, 1, 1);
                s.short_name = "bar#1".to_string();
                s
            }],
            vec![],
        );
        let new = make_parse(
            vec![{
                let mut s =
                    make_sym_full("Foo::bar#2", SymbolKind::Function, Some("h"), None, 1, 1);
                s.short_name = "bar#2".to_string();
                s
            }],
            vec![],
        );
        let changes = classify(&old, &new, source, source);
        // Short names differ so it's detected as a rename
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].change, ChangeKind::Renamed);
    }
}
