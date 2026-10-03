use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::error::Result;
use crate::git::{self, DiffFileEntry};
use crate::parser::adapter::language_for_path;
use crate::parser::{
    self, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, SymbolKind, flatten_symbols,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
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

impl ChangeKind {
    /// The serialized name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::SignatureChanged => "signature_changed",
            Self::BodyChanged => "body_changed",
            Self::CosmeticChanged => "cosmetic_changed",
            Self::Renamed => "renamed",
            Self::Moved => "moved",
        }
    }
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
    span_content(source, sym.start_line, sym.end_line)
}

fn span_content(source: &str, start_line: usize, end_line: usize) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let start = start_line.saturating_sub(1);
    let end = end_line.min(lines.len());
    lines[start..end].join("\n")
}

/// Identity and 1-based line span of a symbol, as rename resolution needs it.
#[derive(Debug, Clone, Copy)]
struct SymbolSpan<'a> {
    qualified_name: &'a str,
    short_name: &'a str,
    kind: &'a str,
    structural_hash: Option<&'a str>,
    start_line: usize,
    end_line: usize,
}

impl<'a> From<&'a ExtractedSymbol> for SymbolSpan<'a> {
    fn from(sym: &'a ExtractedSymbol) -> Self {
        SymbolSpan {
            qualified_name: &sym.qualified_name,
            short_name: &sym.short_name,
            kind: sym.kind.as_str(),
            structural_hash: sym.structural_hash.as_deref(),
            start_line: sym.start_line,
            end_line: sym.end_line,
        }
    }
}

impl UnmatchedSymbol {
    /// A rename-resolution candidate for `sym`, whose span is read from
    /// `source`; `file` is the path the candidate is attributed to.
    fn new(sym: SymbolSpan<'_>, source: &str, file: &str) -> Self {
        let content = span_content(source, sym.start_line, sym.end_line);
        UnmatchedSymbol {
            qualified_name: sym.qualified_name.to_string(),
            short_name: sym.short_name.to_string(),
            kind: sym.kind.to_string(),
            file: file.to_string(),
            body_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
            structural_hash: sym.structural_hash.map(str::to_string),
            start_line: sym.start_line,
            content,
        }
    }
}

fn body_hash(source: &str, sym: &ExtractedSymbol) -> String {
    let content = extract_content(source, sym);
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

/// Names called on the lines `on_line` keeps.
fn callees_where(refs: &[ExtractedRef], on_line: impl Fn(usize) -> bool) -> BTreeSet<&str> {
    refs.iter()
        .filter(|r| r.context_kind == RefContextKind::Call && on_line(r.line))
        .map(|r| r.name.as_str())
        .collect()
}

/// The callees `new` gained and lost over `old`; `None` when neither.
fn diff_callees(old: &BTreeSet<&str>, new: &BTreeSet<&str>) -> Option<CalleeDiff> {
    let cd = CalleeDiff {
        added: new.difference(old).copied().map(String::from).collect(),
        removed: old.difference(new).copied().map(String::from).collect(),
    };
    (!cd.added.is_empty() || !cd.removed.is_empty()).then_some(cd)
}

fn callee_diff(
    old_refs: &[ExtractedRef],
    new_refs: &[ExtractedRef],
    old_sym: &ExtractedSymbol,
    new_sym: &ExtractedSymbol,
) -> Option<CalleeDiff> {
    let in_span = |sym: &ExtractedSymbol| {
        let (start, end) = (sym.start_line, sym.end_line);
        move |l: usize| l >= start && l <= end
    };
    diff_callees(
        &callees_where(old_refs, in_span(old_sym)),
        &callees_where(new_refs, in_span(new_sym)),
    )
}

fn parent_qualified<'a>(qualified_name: &'a str, short_name: &str) -> Option<&'a str> {
    qualified_name
        .strip_suffix(short_name)
        .and_then(|prefix| prefix.strip_suffix("::"))
}

/// Every symbol of one side of a file as a rename/move candidate: the side of
/// a file the diff added or deleted, which `classify_symbols` never sees.
pub fn build_unmatched(parse: &ParseResult, source: &str, file: &str) -> Vec<UnmatchedSymbol> {
    let flat = flatten_symbols(&parse.symbols);
    flat.iter()
        .map(|sym| UnmatchedSymbol::new(SymbolSpan::from(*sym), source, file))
        .collect()
}

/// What a container symbol's (an `impl`, module, struct or class) body is
/// compared on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerScope {
    /// The whole span, members included: a container changes whenever one of
    /// its members does.
    Whole,
    /// Only the container's own lines, outside its members: a member's change
    /// is reported on the member alone, and the container only when its own
    /// text changed (sutra/517).
    Own,
}

/// The 1-based line ranges a symbol's direct members cover. A member takes
/// the contiguous non-blank lines just above it too (its doc comments and
/// attributes, which tree-sitter leaves outside its node), stopping at the
/// container's first line and the previous member's last.
fn member_ranges(sym: &ExtractedSymbol, lines: &[&str]) -> Vec<(usize, usize)> {
    let mut children: Vec<&ExtractedSymbol> = sym.children.iter().collect();
    children.sort_by_key(|c| c.start_line);
    let mut ranges = Vec::with_capacity(children.len());
    let mut floor = sym.start_line;
    for child in children {
        let mut start = child.start_line;
        while start > floor + 1 && lines.get(start - 2).is_some_and(|l| !l.trim().is_empty()) {
            start -= 1;
        }
        ranges.push((start, child.end_line));
        floor = floor.max(child.end_line);
    }
    ranges
}

/// Whether line `l` of `sym` is its own, not a member's.
fn own_line(members: &[(usize, usize)], l: usize) -> bool {
    !members.iter().any(|&(s, e)| (s..=e).contains(&l))
}

/// A container's own lines, trimmed and without blank lines.
fn own_text(lines: &[&str], sym: &ExtractedSymbol, members: &[(usize, usize)]) -> String {
    (sym.start_line..=sym.end_line)
        .filter(|&l| own_line(members, l))
        .filter_map(|l| lines.get(l - 1).map(|t| t.trim()))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A container compared on its own lines ([`ContainerScope::Own`]): `None`
/// when only its members changed.
fn classify_container(
    old_sym: &ExtractedSymbol,
    new_sym: &ExtractedSymbol,
    parses: (&ParseResult, &ParseResult),
    sources: (&str, &str),
) -> Option<SymbolChange> {
    let change = |change: ChangeKind, callee_diff: Option<CalleeDiff>| SymbolChange {
        symbol: new_sym.qualified_name.to_string(),
        kind: new_sym.kind.as_str().to_string(),
        change,
        callee_diff,
        from_symbol: None,
        from_file: None,
    };
    if signature_changed(old_sym, new_sym) {
        return Some(change(ChangeKind::SignatureChanged, None));
    }
    let old_lines: Vec<&str> = sources.0.lines().collect();
    let new_lines: Vec<&str> = sources.1.lines().collect();
    let old_members = member_ranges(old_sym, &old_lines);
    let new_members = member_ranges(new_sym, &new_lines);
    if own_text(&old_lines, old_sym, &old_members) == own_text(&new_lines, new_sym, &new_members) {
        return None;
    }
    let own = |sym: &ExtractedSymbol, members: &[(usize, usize)], l: usize| {
        l >= sym.start_line && l <= sym.end_line && own_line(members, l)
    };
    let cd = diff_callees(
        &callees_where(&parses.0.references, |l| own(old_sym, &old_members, l)),
        &callees_where(&parses.1.references, |l| own(new_sym, &new_members, l)),
    );
    Some(change(ChangeKind::BodyChanged, cd))
}

fn signature_changed(old_sym: &ExtractedSymbol, new_sym: &ExtractedSymbol) -> bool {
    match (&old_sym.signature_hash, &new_sym.signature_hash) {
        (Some(oh), Some(nh)) => oh != nh,
        (None, Some(_)) | (Some(_), None) => true,
        (None, None) => false,
    }
}

/// For each new-side symbol, the old-side symbol it continues, by
/// `(qualified_name, kind)`; and which old-side symbols were continued. A key
/// several symbols share (Rust's `impl Foo` blocks, `Foo::fmt` in two trait
/// impls) pairs identical bodies first and the rest in source order. Keyed
/// through a map instead, every such block was compared against the one block
/// the map kept, and reported a spurious change (sutra/517).
fn pair_symbols(
    old_flat: &[&ExtractedSymbol],
    new_flat: &[&ExtractedSymbol],
    sources: (&str, &str),
) -> (Vec<Option<usize>>, Vec<bool>) {
    type SymKey<'a> = (&'a str, &'a str);
    fn sym_key(s: &ExtractedSymbol) -> SymKey<'_> {
        (s.qualified_name.as_str(), s.kind.as_str())
    }

    let mut groups: HashMap<SymKey<'_>, (Vec<usize>, Vec<usize>)> = HashMap::new();
    for (i, s) in old_flat.iter().enumerate() {
        groups.entry(sym_key(s)).or_default().0.push(i);
    }
    for (i, s) in new_flat.iter().enumerate() {
        groups.entry(sym_key(s)).or_default().1.push(i);
    }

    let mut new_to_old: Vec<Option<usize>> = vec![None; new_flat.len()];
    let mut old_paired = vec![false; old_flat.len()];
    for (olds, news) in groups.values() {
        if let ([o], [n]) = (olds.as_slice(), news.as_slice()) {
            new_to_old[*n] = Some(*o);
            old_paired[*o] = true;
            continue;
        }
        let old_hashes: Vec<String> = olds
            .iter()
            .map(|&o| body_hash(sources.0, old_flat[o]))
            .collect();
        for &n in news {
            let hash = body_hash(sources.1, new_flat[n]);
            if let Some(&o) = olds
                .iter()
                .zip(&old_hashes)
                .find(|&(&o, h)| !old_paired[o] && *h == hash)
                .map(|(o, _)| o)
            {
                new_to_old[n] = Some(o);
                old_paired[o] = true;
            }
        }
        let rest_old: Vec<usize> = olds.iter().filter(|&&o| !old_paired[o]).copied().collect();
        let rest_new: Vec<usize> = news
            .iter()
            .filter(|&&n| new_to_old[n].is_none())
            .copied()
            .collect();
        for (n, o) in rest_new.into_iter().zip(rest_old) {
            new_to_old[n] = Some(o);
            old_paired[o] = true;
        }
    }
    (new_to_old, old_paired)
}

/// Classify the symbols of a file changed on both sides. `sources` and
/// `files` are `(old, new)`.
pub fn classify_symbols(
    old_parse: &ParseResult,
    new_parse: &ParseResult,
    sources: (&str, &str),
    files: (&str, &str),
    containers: ContainerScope,
) -> ClassifyResult {
    let (old_source, new_source) = sources;
    let (old_file, new_file) = files;
    let old_flat = flatten_symbols(&old_parse.symbols);
    let new_flat = flatten_symbols(&new_parse.symbols);
    let (new_to_old, old_paired) = pair_symbols(&old_flat, &new_flat, sources);

    let mut changes = Vec::new();
    let mut unmatched_new = Vec::new();

    for (new_sym, paired) in new_flat.iter().zip(&new_to_old) {
        match paired.map(|o| old_flat[o]) {
            None => {
                unmatched_new.push(UnmatchedSymbol::new(
                    SymbolSpan::from(*new_sym),
                    new_source,
                    new_file,
                ));
            }
            Some(old_sym)
                if containers == ContainerScope::Own
                    && !(old_sym.children.is_empty() && new_sym.children.is_empty()) =>
            {
                changes.extend(classify_container(
                    old_sym,
                    new_sym,
                    (old_parse, new_parse),
                    sources,
                ));
            }
            Some(old_sym) => {
                if signature_changed(old_sym, new_sym) {
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

    let unmatched_old = old_flat
        .iter()
        .zip(&old_paired)
        .filter(|(_, paired)| !**paired)
        .map(|(old_sym, _)| UnmatchedSymbol::new(SymbolSpan::from(*old_sym), old_source, old_file))
        .collect();

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
            let result = classify_symbols(
                &old_parse,
                &new_parse,
                (&old_src, &new_src),
                (path, path),
                ContainerScope::Whole,
            );

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

/// Per-file symbol changes between `base` and `head`, with renames and moves
/// resolved across files. `head` is read as [`git::file_content_on_side`]
/// reads it: `None` is the worktree, `Some("")` the index. Containers are
/// compared on their own lines ([`ContainerScope::Own`]), and the fields of a
/// struct, enum or class the diff added or deleted are folded into it.
pub fn diff_files(
    workspace_root: &Path,
    entries: &[DiffFileEntry],
    base: &str,
    head: Option<&str>,
) -> DiffFilesResult {
    let mut all_unmatched_old: Vec<UnmatchedSymbol> = Vec::new();
    let mut all_unmatched_new: Vec<UnmatchedSymbol> = Vec::new();
    let mut per_file: HashMap<String, Vec<SymbolChange>> = HashMap::new();
    let mut errors: HashMap<String, String> = HashMap::new();

    for entry in entries {
        let old_file = entry.base_path();
        let new_file = &entry.path;

        let language = match language_for_path(new_file).or_else(|| language_for_path(old_file)) {
            Some(l) => l,
            None => continue,
        };

        let mut process = || -> Result<()> {
            let old_source = git::file_content_on_side(workspace_root, Some(base), old_file)?;
            let new_source = git::file_content_on_side(workspace_root, head, new_file)?;

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
                        &old_parse,
                        &new_parse,
                        (&old_src, &new_src),
                        (new_file, new_file),
                        ContainerScope::Own,
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

    for changes in per_file.values_mut() {
        fold_fields(changes);
    }
    DiffFilesResult { per_file, errors }
}

/// Drop the added or deleted fields of a type the same change added or
/// deleted: the type's entry already says it.
fn fold_fields(changes: &mut Vec<SymbolChange>) {
    let whole: HashSet<(&str, ChangeKind)> = changes
        .iter()
        .filter(|c| matches!(c.change, ChangeKind::Added | ChangeKind::Deleted))
        .filter(|c| c.kind != SymbolKind::Field.as_str())
        .map(|c| (c.symbol.as_str(), c.change))
        .collect();
    let folded: Vec<bool> = changes
        .iter()
        .map(|c| {
            c.kind == SymbolKind::Field.as_str()
                && c.symbol
                    .rsplit_once("::")
                    .is_some_and(|(parent, _)| whole.contains(&(parent, c.change)))
        })
        .collect();
    let mut folded = folded.into_iter();
    changes.retain(|_| !folded.next().unwrap_or(false));
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
            receiver: None,
            qualifier: None,
        }
    }

    fn classify(
        old: &ParseResult,
        new: &ParseResult,
        old_src: &str,
        new_src: &str,
    ) -> Vec<SymbolChange> {
        let result = classify_symbols(
            old,
            new,
            (old_src, new_src),
            ("test.rs", "test.rs"),
            ContainerScope::Whole,
        );
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
            (source, ""),
            ("a.rs", "a.rs"),
            ContainerScope::Whole,
        );
        let new_result = classify_symbols(
            &make_parse(vec![], vec![]),
            &new_parse,
            ("", source),
            ("b.rs", "b.rs"),
            ContainerScope::Whole,
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

    #[test]
    fn test_language_for_path_rust() {
        assert_eq!(super::language_for_path("src/main.rs"), Some("rust"));
    }

    #[test]
    fn test_language_for_path_dart() {
        assert_eq!(super::language_for_path("lib/widget.dart"), Some("dart"));
    }

    #[test]
    fn test_language_for_path_c() {
        assert_eq!(super::language_for_path("src/parser.c"), Some("c"));
        assert_eq!(super::language_for_path("include/parser.h"), Some("c"));
    }

    #[test]
    fn test_language_for_path_python() {
        assert_eq!(super::language_for_path("scripts/build.py"), Some("python"));
    }

    #[test]
    fn test_language_for_path_js_ts() {
        // sutra/526: JS/TS were missing from the old literal table, so
        // diff_file silently returned no changes for them.
        assert_eq!(super::language_for_path("web/index.js"), Some("javascript"));
        assert_eq!(super::language_for_path("web/App.jsx"), Some("javascript"));
        assert_eq!(super::language_for_path("web/index.ts"), Some("typescript"));
        assert_eq!(super::language_for_path("web/App.tsx"), Some("typescript"));
    }

    #[test]
    fn test_language_for_path_unknown() {
        assert_eq!(super::language_for_path("readme.md"), None);
        assert_eq!(super::language_for_path("no_extension"), None);
    }

    fn classify_rust(old: &str, new: &str, containers: ContainerScope) -> ClassifyResult {
        let old_parse = parser::parse_file(old, "rust", "a.rs").expect("old side parses");
        let new_parse = parser::parse_file(new, "rust", "a.rs").expect("new side parses");
        classify_symbols(
            &old_parse,
            &new_parse,
            (old, new),
            ("a.rs", "a.rs"),
            containers,
        )
    }

    fn summary(changes: &[SymbolChange]) -> Vec<(&str, &str, ChangeKind)> {
        changes
            .iter()
            .map(|c| (c.symbol.as_str(), c.kind.as_str(), c.change))
            .collect()
    }

    const TWO_IMPLS: &str = "struct Foo;\n\
        impl Foo {\n    fn a(&self) {\n        one();\n    }\n}\n\n\
        impl Foo {\n    fn b(&self) {\n        two();\n    }\n}\n";

    /// sutra/517: same-named impl blocks were keyed through a map, so every
    /// block was compared against the last one and reported the same
    /// spurious callee diff.
    #[test]
    fn same_named_impl_blocks_pair_with_their_own_counterpart() {
        let new = TWO_IMPLS.replace("two()", "three()");
        let result = classify_rust(TWO_IMPLS, &new, ContainerScope::Whole);
        assert_eq!(
            summary(&result.changes),
            vec![
                ("Foo", "impl", ChangeKind::BodyChanged),
                ("Foo::b", "method", ChangeKind::BodyChanged),
            ],
            "only the second block changed"
        );
        for c in &result.changes {
            let cd = c
                .callee_diff
                .as_ref()
                .expect("both changes swapped a callee");
            assert_eq!(cd.added, vec!["three"]);
            assert_eq!(cd.removed, vec!["two"]);
        }
        assert!(result.unmatched_old.is_empty() && result.unmatched_new.is_empty());
    }

    #[test]
    fn an_added_impl_block_of_an_existing_name_is_unmatched_not_changed() {
        let old = "struct Foo;\nimpl Foo {\n    fn a(&self) {}\n}\n";
        let new = format!("{old}\nimpl Foo {{\n    fn b(&self) {{}}\n}}\n");
        let result = classify_rust(old, &new, ContainerScope::Whole);
        assert!(result.changes.is_empty(), "{:?}", summary(&result.changes));
        let added: Vec<&str> = result
            .unmatched_new
            .iter()
            .map(|u| u.kind.as_str())
            .collect();
        assert_eq!(added, vec!["impl", "method"]);
    }

    #[test]
    fn own_scope_reports_a_member_change_on_the_member_alone() {
        let new = TWO_IMPLS.replace("two()", "three()");
        let result = classify_rust(TWO_IMPLS, &new, ContainerScope::Own);
        assert_eq!(
            summary(&result.changes),
            vec![("Foo::b", "method", ChangeKind::BodyChanged)]
        );
    }

    #[test]
    fn own_scope_ignores_a_member_doc_comment_but_not_the_containers_own_lines() {
        let old = "mod m {\n    use a::x;\n\n    fn f() {\n        x();\n    }\n}\n";
        let documented = old.replace("    fn f()", "    /// Calls x.\n    fn f()");
        let result = classify_rust(old, &documented, ContainerScope::Own);
        assert!(
            result.changes.iter().all(|c| c.kind != "module"),
            "a member's doc comment is the member's: {:?}",
            summary(&result.changes)
        );

        let imported = old.replace("use a::x;", "use a::x;\n    use a::y;");
        let result = classify_rust(old, &imported, ContainerScope::Own);
        assert_eq!(
            summary(&result.changes),
            vec![("m", "module", ChangeKind::BodyChanged)]
        );
    }

    #[test]
    fn fields_of_an_added_type_fold_into_it() {
        let change = |symbol: &str, kind: &str, change| SymbolChange {
            symbol: symbol.to_string(),
            kind: kind.to_string(),
            change,
            callee_diff: None,
            from_symbol: None,
            from_file: None,
        };
        let mut changes = vec![
            change("Encode", "struct", ChangeKind::Added),
            change("Encode::first", "field", ChangeKind::Added),
            change("Corpus::always_eligible", "field", ChangeKind::Added),
            change("Old::gone", "field", ChangeKind::Deleted),
            change("Old", "struct", ChangeKind::Deleted),
        ];
        fold_fields(&mut changes);
        assert_eq!(
            summary(&changes),
            vec![
                ("Encode", "struct", ChangeKind::Added),
                ("Corpus::always_eligible", "field", ChangeKind::Added),
                ("Old", "struct", ChangeKind::Deleted),
            ]
        );
    }
}
