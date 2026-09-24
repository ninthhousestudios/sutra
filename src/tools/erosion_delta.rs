//! Diff-scoped erosion delta for `sutra_review` (sutra/451).
//!
//! Parses the base and head side of every changed file and compares their
//! erosion samples. Selection is [`erosion::parsed_samples`], the same rule
//! `sutra_file_health` applies to the index, fed rows flattened exactly as the
//! index persists them — so a function's sample here is the one file_health
//! would report for the same bytes.
//!
//! Functions are paired across the diff by a `(qualified_name, kind)` key unique
//! on both sides of a file first, then by [`symbol_diff::resolve_renames`] over
//! the leftovers of every file (repeated keys, renames, moves across files). An
//! unpaired function is added or deleted. Each side is parsed by its own path's
//! adapter, so a rename across extensions is measured as the index saw it.
//!
//! A side that cannot be read or parsed makes that file's delta unavailable,
//! with a reason, and marks the totals partial — never zero.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Duration;

use serde_json::json;

use crate::git::{self, DiffFileEntry};
use crate::health::erosion::{self, COGNITIVE_THRESHOLD, ErosionAggregate, FunctionSample};
use crate::health::scoring::round2;
use crate::parser::ParseResult;
use crate::parser::adapter::{LanguageAdapter, LanguageRegistry, ParserPool};
use crate::parser::persist::{MAX_LINES, flatten_symbols_for_insert};
use crate::tools::symbol_diff::{self, SymbolSpan, UnmatchedSymbol};

const PARSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Functions listed in the JSON block, largest eroded-mass change first.
const MAX_FUNCTIONS: usize = 20;

/// One side of a changed file.
enum Side<'r> {
    /// The file does not exist on this side (added or deleted).
    Absent,
    /// No language adapter handles this side's path (one end of a rename across
    /// extensions): the index holds no samples for it, so it contributes none.
    Unindexed,
    Unavailable(String),
    Parsed {
        source: String,
        parse: ParseResult,
        /// Language of this side's own path; a rename can change it.
        language: &'r str,
    },
}

struct ChangedFile<'e, 'r> {
    entry: &'e DiffFileEntry,
    base: Side<'r>,
    head: Side<'r>,
}

/// Why a changed file has no comparable delta: one side could not be read or
/// parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct Unavailability {
    /// `"base"`, `"head"` or `"both"`.
    pub side: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum FileOutcome {
    Measured {
        base: ErosionAggregate,
        head: ErosionAggregate,
        /// Summed eroded-mass increases of this file's functions.
        eroded_mass_added: f64,
        /// Summed eroded-mass decreases of this file's functions (positive).
        eroded_mass_removed: f64,
        /// Set when a side parsed with syntax errors: the values are what the
        /// index would hold for those bytes, but complexity may be under-counted.
        partial: Option<String>,
    },
    Unavailable(Unavailability),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileDelta {
    pub path: String,
    pub old_path: Option<String>,
    pub outcome: FileOutcome,
}

/// How a function's erosion changed across the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErosionChange {
    /// Below the threshold at base, at or above it at head.
    CrossedUp,
    CrossedDown,
    /// Eroded on both sides, with a different mass.
    ErodedChanged,
    /// New at head and eroded.
    AddedEroded,
    /// Eroded at base and gone at head.
    DeletedEroded,
}

impl ErosionChange {
    pub fn as_str(self) -> &'static str {
        match self {
            ErosionChange::CrossedUp => "crossed_up",
            ErosionChange::CrossedDown => "crossed_down",
            ErosionChange::ErodedChanged => "eroded_changed",
            ErosionChange::AddedEroded => "added_eroded",
            ErosionChange::DeletedEroded => "deleted_eroded",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FunctionDelta {
    /// Head-side file and name, or the base side's for a deleted function.
    pub file: String,
    pub symbol: String,
    /// Base-side name/file when the function was renamed or moved.
    pub from_symbol: Option<String>,
    pub from_file: Option<String>,
    pub base: Option<FunctionSample>,
    pub head: Option<FunctionSample>,
    /// sutra/404 hook: "fixing this gains X". Erosion is not a health-score
    /// input today, so there is no score marginal to report; when one exists,
    /// fill it here and [`delta_json`] emits it.
    pub marginal_gain: Option<f64>,
}

impl FunctionDelta {
    pub fn change(&self) -> Option<ErosionChange> {
        let eroded = |s: Option<FunctionSample>| s.is_some_and(|s| erosion::is_eroded(s.cognitive));
        match (self.base, self.head) {
            (None, Some(_)) if eroded(self.head) => Some(ErosionChange::AddedEroded),
            (Some(_), None) if eroded(self.base) => Some(ErosionChange::DeletedEroded),
            (Some(b), Some(h)) => match (eroded(self.base), eroded(self.head)) {
                (false, true) => Some(ErosionChange::CrossedUp),
                (true, false) => Some(ErosionChange::CrossedDown),
                (true, true) if erosion::function_mass(b) != erosion::function_mass(h) => {
                    Some(ErosionChange::ErodedChanged)
                }
                _ => None,
            },
            _ => None,
        }
    }

    pub fn eroded_mass_delta(&self) -> f64 {
        eroded_mass(self.head) - eroded_mass(self.base)
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ErosionDelta {
    pub files: Vec<FileDelta>,
    /// Every paired or unpaired function whose erosion changed
    /// ([`FunctionDelta::change`] is `Some`).
    pub functions: Vec<FunctionDelta>,
}

fn eroded_mass(sample: Option<FunctionSample>) -> f64 {
    match sample {
        Some(s) if erosion::is_eroded(s.cognitive) => erosion::function_mass(s),
        _ => 0.0,
    }
}

/// Compare erosion between `base` and `head` (`Some("")` = index, `None` =
/// worktree) over the changed `entries`. Files no language adapter handles are
/// skipped: they are never indexed, so they carry no erosion anywhere.
pub fn compute(
    workspace_root: &Path,
    entries: &[DiffFileEntry],
    base: &str,
    head: Option<&str>,
    registry: &LanguageRegistry,
) -> ErosionDelta {
    let mut pool = ParserPool::new(PARSE_TIMEOUT);
    let files: Vec<ChangedFile<'_, '_>> = entries
        .iter()
        .filter_map(|entry| {
            let (base_adapter, head_adapter) = side_adapters(registry, entry)?;
            Some(ChangedFile {
                entry,
                base: load_side(
                    workspace_root,
                    Some(base),
                    entry.base_path(),
                    base_adapter,
                    &mut pool,
                ),
                head: load_side(workspace_root, head, &entry.path, head_adapter, &mut pool),
            })
        })
        .collect();
    compare(&files)
}

/// Erosion samples of `source` as the review path computes them — the seam the
/// file_health parity test pins. `None` when no adapter handles `path` or the
/// file cannot be parsed.
pub fn source_samples(
    registry: &LanguageRegistry,
    path: &str,
    source: &str,
) -> Option<Vec<FunctionSample>> {
    let adapter = adapter_for(registry, path)?;
    let mut pool = ParserPool::new(PARSE_TIMEOUT);
    match parse_side(source.to_string(), path, adapter, &mut pool) {
        Side::Parsed {
            source,
            parse,
            language,
        } => Some(
            side_functions(&parse, &source, path, language, 0)
                .into_iter()
                .map(|f| f.sample)
                .collect(),
        ),
        Side::Absent | Side::Unindexed | Side::Unavailable(_) => None,
    }
}

/// A side's language adapter; `None` when its path is not indexed.
type SideAdapter<'r> = Option<&'r dyn LanguageAdapter>;

/// The adapter for each side of `entry`, the base side's from its old path:
/// the index parsed each side by its own path at that revision, so a rename
/// across extensions changes the grammar or drops the side from the index.
/// `None` when neither side is indexed.
fn side_adapters<'r>(
    registry: &'r LanguageRegistry,
    entry: &DiffFileEntry,
) -> Option<(SideAdapter<'r>, SideAdapter<'r>)> {
    let base = adapter_for(registry, entry.base_path());
    let head = adapter_for(registry, &entry.path);
    (base.is_some() || head.is_some()).then_some((base, head))
}

fn adapter_for<'r>(registry: &'r LanguageRegistry, path: &str) -> Option<&'r dyn LanguageAdapter> {
    let ext = Path::new(path).extension()?.to_str()?;
    registry.adapter_for_extension(ext)
}

fn load_side<'r>(
    workspace_root: &Path,
    revision: Option<&str>,
    path: &str,
    adapter: Option<&'r dyn LanguageAdapter>,
    pool: &mut ParserPool,
) -> Side<'r> {
    let Some(adapter) = adapter else {
        return Side::Unindexed;
    };
    match git::file_content_on_side(workspace_root, revision, path) {
        Ok(None) => Side::Absent,
        Ok(Some(source)) => parse_side(source, path, adapter, pool),
        Err(e) => Side::Unavailable(format!("read failed: {e}")),
    }
}

fn parse_side<'r>(
    source: String,
    path: &str,
    adapter: &'r dyn LanguageAdapter,
    pool: &mut ParserPool,
) -> Side<'r> {
    // The pipeline skips oversized files, so the index holds no samples for
    // them either; comparing a parse here would invent a number file_health
    // never shows.
    let lines = source.lines().count();
    if lines > MAX_LINES {
        return Side::Unavailable(format!(
            "{lines} lines exceeds the {MAX_LINES}-line index limit"
        ));
    }
    match pool.parse_with(adapter, &source, path) {
        Ok(parse) => Side::Parsed {
            source,
            parse,
            language: adapter.language_id(),
        },
        Err(e) => Side::Unavailable(format!("parse failed: {e}")),
    }
}

/// One erosion-sampled function on one side of the diff.
struct SideFn<'a> {
    /// Index into the changed-file list.
    file: usize,
    span: SymbolSpan<'a>,
    source: &'a str,
    sample: FunctionSample,
}

fn side_functions<'a>(
    parse: &'a ParseResult,
    source: &'a str,
    path: &str,
    language: &str,
    file: usize,
) -> Vec<SideFn<'a>> {
    let (flat, parents) = flatten_symbols_for_insert(&parse.symbols);
    erosion::parsed_samples(&flat, &parents, path, language)
        .into_iter()
        .map(|(idx, sample)| {
            let p = &flat[idx];
            SideFn {
                file,
                span: SymbolSpan {
                    qualified_name: p.qualified_name,
                    short_name: p.short_name,
                    kind: p.kind,
                    structural_hash: p.structural_hash,
                    start_line: usize::try_from(p.start_line).unwrap_or(0),
                    end_line: usize::try_from(p.end_line).unwrap_or(0),
                },
                source,
                sample,
            }
        })
        .collect()
}

fn side_parse<'s, 'r>(side: &'s Side<'r>) -> Option<(&'s ParseResult, &'s str, &'r str)> {
    match side {
        Side::Parsed {
            source,
            parse,
            language,
        } => Some((parse, source.as_str(), language)),
        Side::Absent | Side::Unindexed | Side::Unavailable(_) => None,
    }
}

fn unavailability(file: &ChangedFile<'_, '_>) -> Option<Unavailability> {
    match (&file.base, &file.head) {
        (Side::Unavailable(b), Side::Unavailable(h)) => Some(Unavailability {
            side: "both",
            reason: format!("base: {b}; head: {h}"),
        }),
        (Side::Unavailable(r), _) => Some(Unavailability {
            side: "base",
            reason: r.to_string(),
        }),
        (_, Side::Unavailable(r)) => Some(Unavailability {
            side: "head",
            reason: r.to_string(),
        }),
        _ => None,
    }
}

fn syntax_errors(file: &ChangedFile<'_, '_>) -> Option<String> {
    let bad = |side: &Side| side_parse(side).is_some_and(|(p, _, _)| !p.parsed_ok);
    let sides: Vec<&str> = [("base", &file.base), ("head", &file.head)]
        .into_iter()
        .filter(|(_, s)| bad(s))
        .map(|(name, _)| name)
        .collect();
    (!sides.is_empty()).then(|| {
        format!(
            "{} parsed with syntax errors; complexity may be under-counted",
            sides.join(" and ")
        )
    })
}

fn compare(files: &[ChangedFile<'_, '_>]) -> ErosionDelta {
    let (olds, news) = sample_sides(files);
    let pairs = pair_functions(files, &olds, &news);
    let mass = attribute_mass(&pairs, &olds, &news);
    ErosionDelta {
        files: file_deltas(files, &olds, &news, &mass),
        functions: function_deltas(files, &pairs, &olds, &news),
    }
}

/// Erosion samples of both sides of every comparable file. An unavailable file
/// joins neither side: pairing its readable half would report it all as added
/// or deleted.
fn sample_sides<'a>(files: &'a [ChangedFile<'_, '_>]) -> (Vec<SideFn<'a>>, Vec<SideFn<'a>>) {
    let mut olds = Vec::new();
    let mut news = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if unavailability(f).is_some() {
            continue;
        }
        let path = f.entry.path.as_str();
        let old_path = f.entry.base_path();
        if let Some((parse, source, language)) = side_parse(&f.base) {
            olds.extend(side_functions(parse, source, old_path, language, i));
        }
        if let Some((parse, source, language)) = side_parse(&f.head) {
            news.extend(side_functions(parse, source, path, language, i));
        }
    }
    (olds, news)
}

/// Per-file eroded-mass increases and decreases, keyed by changed-file index.
#[derive(Debug, Default)]
struct FileMass {
    added: HashMap<usize, f64>,
    /// Positive magnitudes.
    removed: HashMap<usize, f64>,
}

impl FileMass {
    fn record(&mut self, file: usize, delta: f64) {
        if delta > 0.0 {
            *self.added.entry(file).or_default() += delta;
        } else if delta < 0.0 {
            *self.removed.entry(file).or_default() -= delta;
        }
    }
}

/// Attribute each pair's eroded-mass change to files. A function paired within
/// one file contributes its net change; a function moved across files is
/// removed from its base file and added to its head file.
fn attribute_mass(
    pairs: &[(Option<usize>, Option<usize>)],
    olds: &[SideFn<'_>],
    news: &[SideFn<'_>],
) -> FileMass {
    let mut mass = FileMass::default();
    for &(o, n) in pairs {
        let old = o.map(|i| &olds[i]);
        let new = n.map(|i| &news[i]);
        match (old, new) {
            (Some(of), Some(nf)) if of.file == nf.file => {
                mass.record(
                    of.file,
                    eroded_mass(Some(nf.sample)) - eroded_mass(Some(of.sample)),
                );
            }
            _ => {
                if let Some(of) = old {
                    mass.record(of.file, -eroded_mass(Some(of.sample)));
                }
                if let Some(nf) = new {
                    mass.record(nf.file, eroded_mass(Some(nf.sample)));
                }
            }
        }
    }
    mass
}

/// One [`FunctionDelta`] per pair whose erosion changed.
fn function_deltas(
    files: &[ChangedFile<'_, '_>],
    pairs: &[(Option<usize>, Option<usize>)],
    olds: &[SideFn<'_>],
    news: &[SideFn<'_>],
) -> Vec<FunctionDelta> {
    pairs
        .iter()
        .filter_map(|&(o, n)| {
            let old = o.map(|i| &olds[i]);
            let new = n.map(|i| &news[i]);
            let shown = new.or(old)?;
            let renamed = old.zip(new).and_then(|(of, nf)| {
                (of.span.qualified_name != nf.span.qualified_name)
                    .then(|| of.span.qualified_name.to_string())
            });
            let moved = old.zip(new).and_then(|(of, nf)| {
                (of.file != nf.file).then(|| files[of.file].entry.path.to_string())
            });
            let delta = FunctionDelta {
                file: files[shown.file].entry.path.to_string(),
                symbol: shown.span.qualified_name.to_string(),
                from_symbol: renamed,
                from_file: moved,
                base: old.map(|f| f.sample),
                head: new.map(|f| f.sample),
                marginal_gain: None,
            };
            delta.change().is_some().then_some(delta)
        })
        .collect()
}

fn file_deltas(
    files: &[ChangedFile<'_, '_>],
    olds: &[SideFn<'_>],
    news: &[SideFn<'_>],
    mass: &FileMass,
) -> Vec<FileDelta> {
    files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let outcome = match unavailability(f) {
                Some(u) => FileOutcome::Unavailable(u),
                None => FileOutcome::Measured {
                    base: erosion::aggregate(
                        olds.iter().filter(|s| s.file == i).map(|s| &s.sample),
                    ),
                    head: erosion::aggregate(
                        news.iter().filter(|s| s.file == i).map(|s| &s.sample),
                    ),
                    eroded_mass_added: mass.added.get(&i).copied().unwrap_or(0.0),
                    eroded_mass_removed: mass.removed.get(&i).copied().unwrap_or(0.0),
                    partial: syntax_errors(f),
                },
            };
            FileDelta {
                path: f.entry.path.to_string(),
                old_path: f.entry.old_path.as_ref().map(|p| p.to_string()),
                outcome,
            }
        })
        .collect()
}

/// Pair base and head functions. A `(qualified_name, kind)` key naming exactly
/// one function on each side of a changed file pairs directly. A repeated key
/// (cfg-gated twins, same-named items) is ambiguous: source position says
/// nothing about which twin is which, so the group is first resolved within
/// itself — body identity, then same-name similarity — before any leftover
/// joins rename resolution across every file. Resolving globally first let an
/// unrelated function with a twin's structure (the structural hash ignores
/// names) claim that twin. Twins identity cannot tell apart fall back to source
/// order; see docs/health-map.md for that known limit. Returns `(old, new)`
/// index pairs into `olds`/`news`; an unpaired function appears with the other
/// side `None`.
fn pair_functions<'a>(
    files: &[ChangedFile<'_, '_>],
    olds: &[SideFn<'a>],
    news: &[SideFn<'a>],
) -> Vec<(Option<usize>, Option<usize>)> {
    type Key<'a> = (usize, &'a str, &'a str);
    fn key<'a>(f: &SideFn<'a>) -> Key<'a> {
        (f.file, f.span.qualified_name, f.span.kind)
    }
    let mut groups: HashMap<Key<'a>, (Vec<usize>, Vec<usize>)> = HashMap::new();
    for (o, f) in olds.iter().enumerate() {
        groups.entry(key(f)).or_default().0.push(o);
    }
    for (n, f) in news.iter().enumerate() {
        groups.entry(key(f)).or_default().1.push(n);
    }
    // Candidates are attributed to the head path on both sides, so a function
    // in a renamed file still resolves as same-file.
    let candidate =
        |f: &SideFn<'_>| UnmatchedSymbol::new(f.span, f.source, &files[f.file].entry.path);

    let mut pairs: Vec<(Option<usize>, Option<usize>)> = Vec::new();
    let mut leftover_old: Vec<usize> = Vec::new();
    let mut leftover_new: Vec<usize> = Vec::new();
    // Groups in source order (head first), so pair order is deterministic.
    let mut visited: HashSet<Key<'a>> = HashSet::new();
    for f in news.iter().chain(olds) {
        let k = key(f);
        if !visited.insert(k) {
            continue;
        }
        let (os, ns) = groups
            .get(&k)
            .expect("invariant: every sampled function's key is grouped");
        if let ([o], [n]) = (os.as_slice(), ns.as_slice()) {
            pairs.push((Some(*o), Some(*n)));
            continue;
        }
        let group_old: Vec<UnmatchedSymbol> = os.iter().map(|&o| candidate(&olds[o])).collect();
        let group_new: Vec<UnmatchedSymbol> = ns.iter().map(|&n| candidate(&news[n])).collect();
        let within = symbol_diff::resolve_renames(&group_old, &group_new);
        pairs.extend(
            within
                .pairs
                .iter()
                .map(|&(go, gn)| (Some(os[go]), Some(ns[gn]))),
        );
        leftover_old.extend(
            os.iter()
                .enumerate()
                .filter(|(g, _)| !within.matched_old.contains(g))
                .map(|(_, &o)| o),
        );
        leftover_new.extend(
            ns.iter()
                .enumerate()
                .filter(|(g, _)| !within.matched_new.contains(g))
                .map(|(_, &n)| n),
        );
    }
    leftover_old.sort_unstable();
    leftover_new.sort_unstable();

    let unmatched_old: Vec<UnmatchedSymbol> =
        leftover_old.iter().map(|&o| candidate(&olds[o])).collect();
    let unmatched_new: Vec<UnmatchedSymbol> =
        leftover_new.iter().map(|&n| candidate(&news[n])).collect();
    let resolved = symbol_diff::resolve_renames(&unmatched_old, &unmatched_new);

    let mut resolved_old = vec![false; leftover_old.len()];
    let mut resolved_new = vec![false; leftover_new.len()];
    for &(uo, un) in &resolved.pairs {
        resolved_old[uo] = true;
        resolved_new[un] = true;
        pairs.push((Some(leftover_old[uo]), Some(leftover_new[un])));
    }

    // Last resort: twins identity could not separate (all rewritten) pair in
    // source order, as a unique key pairs same-named functions whatever their
    // bodies.
    let mut unresolved_old: HashMap<Key<'a>, VecDeque<usize>> = HashMap::new();
    for (&o, _) in leftover_old.iter().zip(&resolved_old).filter(|(_, r)| !**r) {
        unresolved_old
            .entry(key(&olds[o]))
            .or_default()
            .push_back(o);
    }
    for (&n, _) in leftover_new.iter().zip(&resolved_new).filter(|(_, r)| !**r) {
        let o = unresolved_old
            .get_mut(&key(&news[n]))
            .and_then(VecDeque::pop_front);
        pairs.push((o, Some(n)));
    }
    let mut still_old: Vec<usize> = unresolved_old.into_values().flatten().collect();
    still_old.sort_unstable();
    pairs.extend(still_old.into_iter().map(|o| (Some(o), None)));
    pairs
}

fn side_json(agg: &ErosionAggregate) -> serde_json::Value {
    json!({
        "eroded_mass": round2(agg.eroded_mass),
        "total_mass": round2(agg.total_mass),
        "eroded_count": agg.eroded_count,
        "function_count": agg.function_count,
    })
}

/// The `erosion_delta` block of `sutra_review`. `None` when no changed file has
/// a language adapter (nothing to measure). Complete files with no eroded mass
/// on either side (including path-excluded test files) are omitted from `files`:
/// they carry no erosion signal, and total mass alone is descriptive (sutra/403).
pub fn delta_json(delta: &ErosionDelta) -> Option<serde_json::Value> {
    if delta.files.is_empty() {
        return None;
    }
    let mut base_eroded = 0.0;
    let mut head_eroded = 0.0;
    let mut added = 0.0;
    let mut removed = 0.0;
    let mut unavailable = 0;
    let mut partial = 0;
    let mut files_out: Vec<(f64, serde_json::Value)> = Vec::new();
    for f in &delta.files {
        let mut j = json!({ "path": f.path });
        if let Some(old) = &f.old_path {
            j["old_path"] = json!(old);
        }
        match &f.outcome {
            FileOutcome::Unavailable(u) => {
                unavailable += 1;
                j["status"] = json!("unavailable");
                j["side"] = json!(u.side);
                j["reason"] = json!(u.reason);
                files_out.push((f64::INFINITY, j));
            }
            FileOutcome::Measured {
                base,
                head,
                eroded_mass_added,
                eroded_mass_removed,
                partial: reason,
            } => {
                base_eroded += base.eroded_mass;
                head_eroded += head.eroded_mass;
                added += eroded_mass_added;
                removed += eroded_mass_removed;
                if base.eroded_mass == 0.0 && head.eroded_mass == 0.0 && reason.is_none() {
                    continue;
                }
                let net = head.eroded_mass - base.eroded_mass;
                j["status"] = json!(if reason.is_some() {
                    "partial"
                } else {
                    "complete"
                });
                if let Some(r) = reason {
                    partial += 1;
                    j["reason"] = json!(r);
                }
                if let Some(excluded) = erosion::file_exclusion(&f.path) {
                    j["excluded"] = json!(excluded);
                }
                j["base"] = side_json(base);
                j["head"] = side_json(head);
                j["eroded_mass_delta"] = json!(round2(net));
                j["eroded_mass_added"] = json!(round2(*eroded_mass_added));
                j["eroded_mass_removed"] = json!(round2(*eroded_mass_removed));
                files_out.push((net.abs(), j));
            }
        }
    }
    // Unavailable files first (they hide an unknown change), then by |net|.
    files_out.sort_by(|a, b| b.0.total_cmp(&a.0));

    let mut functions: Vec<&FunctionDelta> = delta.functions.iter().collect();
    functions.sort_by(|a, b| {
        b.eroded_mass_delta()
            .abs()
            .total_cmp(&a.eroded_mass_delta().abs())
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
    let count = |c: ErosionChange| {
        delta
            .functions
            .iter()
            .filter(|f| f.change() == Some(c))
            .count()
    };
    let functions_out: Vec<serde_json::Value> = functions
        .iter()
        .take(MAX_FUNCTIONS)
        .map(|f| function_json(f))
        .collect();

    let mut out = json!({
        "threshold": COGNITIVE_THRESHOLD,
        "status": if unavailable + partial > 0 { "partial" } else { "complete" },
        "total": {
            "base_eroded_mass": round2(base_eroded),
            "head_eroded_mass": round2(head_eroded),
            "eroded_mass_delta": round2(head_eroded - base_eroded),
            "eroded_mass_added": round2(added),
            "eroded_mass_removed": round2(removed),
            "crossed_up": count(ErosionChange::CrossedUp),
            "crossed_down": count(ErosionChange::CrossedDown),
            "added_eroded": count(ErosionChange::AddedEroded),
            "deleted_eroded": count(ErosionChange::DeletedEroded),
        },
        "files": files_out.into_iter().map(|(_, j)| j).collect::<Vec<_>>(),
        "functions": functions_out,
    });
    if unavailable > 0 {
        // Totals sum only the measured files.
        out["unavailable_files"] = json!(unavailable);
    }
    if delta.functions.len() > MAX_FUNCTIONS {
        out["functions_total"] = json!(delta.functions.len());
    }
    Some(out)
}

fn function_json(f: &FunctionDelta) -> serde_json::Value {
    let mut j = json!({
        "file": f.file,
        "symbol": f.symbol,
        "change": f.change().map(ErosionChange::as_str),
        "cognitive_base": f.base.map(|s| s.cognitive),
        "cognitive_head": f.head.map(|s| s.cognitive),
        "eroded_mass_base": round2(eroded_mass(f.base)),
        "eroded_mass_head": round2(eroded_mass(f.head)),
        "eroded_mass_delta": round2(f.eroded_mass_delta()),
    });
    if let Some(s) = &f.from_symbol {
        j["from_symbol"] = json!(s);
    }
    if let Some(p) = &f.from_file {
        j["from_file"] = json!(p);
    }
    if let Some(g) = f.marginal_gain {
        j["marginal_gain"] = json!(round2(g));
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::adapter::default_registry;

    /// A Rust fn whose cognitive score is 1 + 2 + … + depth (nested ifs).
    fn nested(name: &str, depth: usize) -> String {
        let mut body = String::from("0");
        for d in (0..depth).rev() {
            body = format!("if x > {d} {{ {body} }} else {{ 1 }}");
        }
        format!("pub fn {name}(x: i32) -> i32 {{\n    {body}\n}}\n")
    }

    fn entry(path: &str, old_path: Option<&str>) -> DiffFileEntry {
        DiffFileEntry {
            path: path.to_string(),
            old_path: old_path.map(str::to_string),
        }
    }

    /// Mirrors [`load_side`] with `src` standing in for the git read.
    fn side<'r>(
        adapter: Option<&'r dyn LanguageAdapter>,
        path: &str,
        src: Option<&str>,
    ) -> Side<'r> {
        let Some(adapter) = adapter else {
            return Side::Unindexed;
        };
        let mut pool = ParserPool::new(PARSE_TIMEOUT);
        match src {
            Some(s) => parse_side(s.to_string(), path, adapter, &mut pool),
            None => Side::Absent,
        }
    }

    fn run(entries: &[DiffFileEntry], sides: Vec<(Option<&str>, Option<&str>)>) -> ErosionDelta {
        let registry = default_registry();
        let files: Vec<ChangedFile<'_, '_>> = entries
            .iter()
            .zip(sides)
            .map(|(e, (b, h))| {
                let (base, head) = side_adapters(&registry, e).expect("fixture path is indexed");
                ChangedFile {
                    entry: e,
                    base: side(base, e.base_path(), b),
                    head: side(head, &e.path, h),
                }
            })
            .collect();
        compare(&files)
    }

    fn measured(f: &FileDelta) -> (&ErosionAggregate, &ErosionAggregate, f64, f64) {
        match &f.outcome {
            FileOutcome::Measured {
                base,
                head,
                eroded_mass_added,
                eroded_mass_removed,
                ..
            } => (base, head, *eroded_mass_added, *eroded_mass_removed),
            FileOutcome::Unavailable(u) => panic!("unexpected unavailable: {u:?}"),
        }
    }

    fn assert_net_matches(delta: &ErosionDelta) {
        for f in &delta.files {
            if let FileOutcome::Measured { .. } = f.outcome {
                let (b, h, added, removed) = measured(f);
                assert!(
                    ((added - removed) - (h.eroded_mass - b.eroded_mass)).abs() < 1e-9,
                    "{}: added {added} - removed {removed} != net",
                    f.path
                );
            }
        }
    }

    fn side_fn(file: usize, cognitive: i64) -> SideFn<'static> {
        SideFn {
            file,
            span: SymbolSpan {
                qualified_name: "f",
                short_name: "f",
                kind: "function",
                structural_hash: None,
                start_line: 1,
                end_line: 4,
            },
            source: "",
            sample: FunctionSample { cognitive, sloc: 4 },
        }
    }

    #[test]
    fn attribute_mass_nets_in_file_and_splits_across_files() {
        let olds = [side_fn(0, 20), side_fn(0, 20), side_fn(1, 3)];
        let news = [side_fn(0, 30), side_fn(1, 20), side_fn(1, 20)];
        // In-file change, cross-file move, and an added fn over a below-threshold
        // deleted one.
        let pairs = [
            (Some(0), Some(0)),
            (Some(1), Some(1)),
            (Some(2), None),
            (None, Some(2)),
        ];
        let mass = attribute_mass(&pairs, &olds, &news);
        let m = |c: i64| {
            eroded_mass(Some(FunctionSample {
                cognitive: c,
                sloc: 4,
            }))
        };
        assert_eq!(mass.added.get(&0), Some(&(m(30) - m(20))));
        assert_eq!(mass.removed.get(&0), Some(&m(20)));
        assert_eq!(mass.added.get(&1), Some(&(m(20) + m(20))));
        assert_eq!(
            mass.removed.get(&1),
            None,
            "a below-threshold deletion removes no mass"
        );
    }

    #[test]
    fn fixture_depths_straddle_threshold() {
        let registry = default_registry();
        let cog = |src: &str| source_samples(&registry, "a.rs", src).expect("parses")[0].cognitive;
        assert!(!erosion::is_eroded(cog(&nested("f", 3))));
        assert!(erosion::is_eroded(cog(&nested("f", 6))));
    }

    #[test]
    fn threshold_crossings_both_directions() {
        let base = format!("{}{}", nested("up", 3), nested("down", 6));
        let head = format!("{}{}", nested("up", 6), nested("down", 3));
        let delta = run(&[entry("a.rs", None)], vec![(Some(&base), Some(&head))]);
        let change = |name: &str| {
            delta
                .functions
                .iter()
                .find(|f| f.symbol == name)
                .and_then(FunctionDelta::change)
        };
        assert_eq!(change("up"), Some(ErosionChange::CrossedUp));
        assert_eq!(change("down"), Some(ErosionChange::CrossedDown));
        let (_, _, added, removed) = measured(&delta.files[0]);
        assert!(added > 0.0 && removed > 0.0);
        assert_net_matches(&delta);
    }

    #[test]
    fn eroded_on_both_sides_reports_mass_change() {
        let delta = run(
            &[entry("a.rs", None)],
            vec![(Some(&nested("f", 6)), Some(&nested("f", 7)))],
        );
        assert_eq!(delta.functions.len(), 1);
        assert_eq!(
            delta.functions[0].change(),
            Some(ErosionChange::ErodedChanged)
        );
        assert!(delta.functions[0].eroded_mass_delta() > 0.0);
        assert_net_matches(&delta);
    }

    #[test]
    fn added_and_deleted_eroded_functions() {
        let base = format!("{}{}", nested("keep", 1), nested("gone", 6));
        let head = format!("{}{}", nested("keep", 1), nested("fresh", 7));
        let delta = run(&[entry("a.rs", None)], vec![(Some(&base), Some(&head))]);
        let changes: Vec<_> = delta
            .functions
            .iter()
            .map(|f| (f.symbol.as_str(), f.change()))
            .collect();
        assert!(changes.contains(&("gone", Some(ErosionChange::DeletedEroded))));
        assert!(changes.contains(&("fresh", Some(ErosionChange::AddedEroded))));
        assert_net_matches(&delta);
    }

    #[test]
    fn rename_is_paired_not_added_and_deleted() {
        let base = nested("old_name", 6);
        let head = base.replace("old_name", "new_name");
        let delta = run(&[entry("a.rs", None)], vec![(Some(&base), Some(&head))]);
        assert!(
            delta.functions.is_empty(),
            "unchanged renamed fn is no erosion change: {:?}",
            delta.functions
        );
        let (_, _, added, removed) = measured(&delta.files[0]);
        assert_eq!((added, removed), (0.0, 0.0));
    }

    #[test]
    fn rename_with_body_change_is_unpaired_like_symbol_diff() {
        // resolve_renames pairs on body/structural hash, or on the same short
        // name; a rename that also rewrites the body matches neither, exactly as
        // in symbol_diff's own change list.
        let base = nested("old_name", 6);
        let head = nested("new_name", 7);
        let delta = run(&[entry("a.rs", None)], vec![(Some(&base), Some(&head))]);
        let mut changes: Vec<_> = delta.functions.iter().map(FunctionDelta::change).collect();
        changes.sort_by_key(|c| c.map(ErosionChange::as_str));
        assert_eq!(
            changes,
            vec![
                Some(ErosionChange::AddedEroded),
                Some(ErosionChange::DeletedEroded)
            ]
        );
        assert_net_matches(&delta);
    }

    #[test]
    fn move_with_reformat_carries_from_file() {
        let base = nested("mover", 6);
        // Same structure, one extra line: structural hash matches, span grows.
        let head = base.replacen("{\n", "{\n\n", 1);
        let delta = run(
            &[entry("a.rs", None), entry("b.rs", None)],
            vec![(Some(&base), None), (None, Some(&head))],
        );
        assert_eq!(delta.functions.len(), 1, "{:?}", delta.functions);
        let f = &delta.functions[0];
        assert_eq!(f.change(), Some(ErosionChange::ErodedChanged));
        assert_eq!(f.file, "b.rs");
        assert_eq!(f.from_file.as_deref(), Some("a.rs"));
        assert_eq!(f.from_symbol, None);
        assert_net_matches(&delta);
    }

    #[test]
    fn cross_file_move_shifts_mass_between_files() {
        let moved = nested("mover", 6);
        let stay = nested("stay", 1);
        let delta = run(
            &[entry("a.rs", None), entry("b.rs", None)],
            vec![
                (Some(&format!("{stay}{moved}")), Some(&stay)),
                (
                    Some(&stay.replace("stay", "other")),
                    Some(&format!("{}{moved}", stay.replace("stay", "other"))),
                ),
            ],
        );
        assert!(delta.functions.is_empty(), "{:?}", delta.functions);
        let (_, _, a_added, a_removed) = measured(&delta.files[0]);
        let (_, _, b_added, b_removed) = measured(&delta.files[1]);
        assert!(a_removed > 0.0 && a_added == 0.0);
        assert_eq!(b_added, a_removed);
        assert_eq!(b_removed, 0.0);
        assert_net_matches(&delta);
    }

    #[test]
    fn renamed_file_pairs_against_old_path() {
        let src = nested("f", 6);
        let delta = run(
            &[entry("new.rs", Some("old.rs"))],
            vec![(Some(&src), Some(&src))],
        );
        assert!(delta.functions.is_empty());
        let (b, h, added, removed) = measured(&delta.files[0]);
        assert_eq!(b.eroded_mass, h.eroded_mass);
        assert_eq!((added, removed), (0.0, 0.0));
    }

    #[test]
    fn unreadable_side_is_unavailable_never_zero() {
        let registry = default_registry();
        let e = entry("a.rs", None);
        let src = nested("f", 6);
        let files = vec![ChangedFile {
            entry: &e,
            base: side(adapter_for(&registry, "a.rs"), "a.rs", Some(&src)),
            head: Side::Unavailable("read failed: boom".to_string()),
        }];
        let delta = compare(&files);
        assert!(delta.functions.is_empty(), "no half-paired deletions");
        assert_eq!(
            delta.files[0].outcome,
            FileOutcome::Unavailable(Unavailability {
                side: "head",
                reason: "read failed: boom".to_string(),
            })
        );
        let j = delta_json(&delta).expect("block");
        assert_eq!(j["status"], "partial");
        assert_eq!(j["unavailable_files"], 1);
        assert_eq!(j["files"][0]["status"], "unavailable");
        assert!(j["files"][0].get("eroded_mass_delta").is_none());
    }

    #[test]
    fn syntax_errors_mark_file_partial() {
        let base = nested("f", 6);
        let head = format!("{}\nfn broken( {{", nested("f", 6));
        let delta = run(&[entry("a.rs", None)], vec![(Some(&base), Some(&head))]);
        let j = delta_json(&delta).expect("block");
        assert_eq!(j["status"], "partial");
        assert_eq!(j["files"][0]["status"], "partial");
    }

    #[test]
    fn no_adapter_files_emit_no_block() {
        assert!(delta_json(&ErosionDelta::default()).is_none());
    }

    /// Two `f`s gated on complementary cfgs: one below the threshold, one
    /// above, in the given order.
    fn cfg_twins(first: (&str, usize), second: (&str, usize)) -> String {
        format!(
            "#[cfg({})]\n{}#[cfg({})]\n{}",
            first.0,
            nested("f", first.1),
            second.0,
            nested("f", second.1)
        )
    }

    #[test]
    fn reordered_cfg_twins_are_no_erosion_change() {
        let base = cfg_twins(("unix", 3), ("not(unix)", 6));
        let head = cfg_twins(("not(unix)", 6), ("unix", 3));
        let delta = run(&[entry("a.rs", None)], vec![(Some(&base), Some(&head))]);
        assert!(
            delta.functions.is_empty(),
            "reordering unchanged twins fabricated changes: {:?}",
            delta.functions
        );
        let (_, _, added, removed) = measured(&delta.files[0]);
        assert_eq!((added, removed), (0.0, 0.0));
    }

    #[test]
    fn reordered_edited_twin_pairs_by_similarity_not_position() {
        let base = cfg_twins(("unix", 3), ("not(unix)", 6));
        let head = cfg_twins(("not(unix)", 7), ("unix", 3));
        let delta = run(&[entry("a.rs", None)], vec![(Some(&base), Some(&head))]);
        let changes: Vec<_> = delta.functions.iter().map(FunctionDelta::change).collect();
        assert_eq!(changes, vec![Some(ErosionChange::ErodedChanged)]);
        assert_net_matches(&delta);
    }

    #[test]
    fn unrelated_structural_twin_cannot_claim_a_twins_identity() {
        // g is new and structurally identical to the eroded f's old body (the
        // structural hash ignores names); it precedes the edited f.
        let base = cfg_twins(("unix", 3), ("not(unix)", 6));
        let head = format!(
            "{}{}",
            nested("g", 6),
            cfg_twins(("unix", 3), ("not(unix)", 7))
        );
        let delta = run(&[entry("a.rs", None)], vec![(Some(&base), Some(&head))]);
        let change = |name: &str| {
            delta
                .functions
                .iter()
                .filter(|f| f.symbol == name)
                .map(FunctionDelta::change)
                .collect::<Vec<_>>()
        };
        assert_eq!(change("g"), vec![Some(ErosionChange::AddedEroded)]);
        assert_eq!(change("f"), vec![Some(ErosionChange::ErodedChanged)]);
        assert_net_matches(&delta);
    }

    #[test]
    fn rename_into_supported_language_adds_its_erosion() {
        let src = nested("f", 6);
        let delta = run(
            &[entry("a.rs", Some("a.txt"))],
            vec![(Some(&src), Some(&src))],
        );
        let (b, h, added, _) = measured(&delta.files[0]);
        assert_eq!(b.function_count, 0, "the index never parsed a.txt");
        assert_eq!(h.eroded_count, 1);
        assert!(added > 0.0);
        assert_eq!(
            delta.functions[0].change(),
            Some(ErosionChange::AddedEroded)
        );
    }

    #[test]
    fn rename_out_of_supported_language_deletes_its_erosion() {
        let src = nested("f", 6);
        let delta = run(
            &[entry("a.txt", Some("a.rs"))],
            vec![(Some(&src), Some(&src))],
        );
        let (b, h, _, removed) = measured(&delta.files[0]);
        assert_eq!(b.eroded_count, 1);
        assert_eq!(h.function_count, 0, "the index no longer parses a.txt");
        assert!(removed > 0.0);
        assert_eq!(
            delta.functions[0].change(),
            Some(ErosionChange::DeletedEroded)
        );
    }

    #[test]
    fn cross_language_rename_parses_each_side_with_its_own_grammar() {
        let rust = nested("f", 6);
        let python = "def g(x):\n    return x\n";
        let delta = run(
            &[entry("a.py", Some("a.rs"))],
            vec![(Some(&rust), Some(python))],
        );
        let (b, _, _, _) = measured(&delta.files[0]);
        assert_eq!(b.eroded_count, 1, "base must be parsed as Rust, not Python");
    }
}
