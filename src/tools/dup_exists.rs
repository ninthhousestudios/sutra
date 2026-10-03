//! "This already exists": the dup-exists advisory (sutra/469), for the DUP
//! failure mode (logic re-implemented instead of reused, then the copies drift).
//!
//! Designed and back-tested in `docs/dup-exists-backtest.md`. The unit is the
//! code a change adds: every function it adds, and the added lines of every
//! function it modifies (4 of the 9 detectable introductions copied a block
//! into an existing function). Each unit is scored against every other
//! non-test function in the same language on three channels:
//!
//! - embed: HRR cosine on AST shape plus identifiers (the whole function,
//!   even for a modified one: vectors exist only per symbol);
//! - lex: tf-idf cosine over the identifier subtokens of the body, signature
//!   dropped;
//! - block: shared 12-token runs that occur in at most 3 corpus functions, so
//!   a block copied into a larger function shows where whole-function cosine
//!   is diluted, and common idioms do not.
//!
//! A match fires on `block >= 6` or `(embed + lex) / 2 >= 0.5`. A modified
//! function fires on block alone, counting only runs its pre-change body did
//! not hold: its embed vector is the whole function, which on the samples
//! matched every sibling of a lightly edited `build` (the three modified
//! back-test cases all fire on block). The corpus is
//! the index, which holds the worktree: the post-change tree. So a match the
//! change moved or deleted is gone by construction, and added code is
//! compared with the other code the same change added (sutra/456, 438, ai/197
//! landed both copies in one commit). A match the new code calls is reuse
//! (`delegates`); a match the change edited to call the new code, or that
//! lost the shared runs, is the extraction itself (`extracted`). Both are
//! dropped.
//!
//! Advisory, never gating, and it names what exists, never "reuse X":
//! over-reuse is a real counter-mode. A search cut short by a cap reports
//! `incomplete`, never clean.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::json;
use tree_sitter::{Point, Tree};

use crate::db::firings::{FiringContext, FiringRecord};
use crate::db::{CorpusFunction, Db, RefRow};
use crate::error::Result;
use crate::git;
use crate::lexical_tokenize::tokenize;
use crate::parser::adapter::{LanguageRegistry, ParserPool};
use crate::parser::{self, ParseResult, RefContextKind, flatten_symbols};
use crate::similarity::hrr::HrrVec;
use crate::similarity::{MAX_HRR_SYMBOL_LINES, SimilarityMode};
use crate::tools::advisory::{self, dirty_outside_diff, index_mismatch};
use crate::tools::firings::ReviewedPatch;
use crate::tools::orphans::qualifier_fits;
use crate::tools::review::DiffScope;
use crate::tools::sibling_pattern::read_sides;
use crate::tools::symbol_diff::{
    ChangeKind, ContainerScope, UnmatchedSymbol, build_unmatched, classify_symbols, resolve_renames,
};

/// The mechanism name in the firing log.
pub const MECHANISM: &str = "dup_exists";

/// Functions shorter than this, and modified functions that gained fewer
/// non-blank lines, are not units; shorter corpus functions are not matches.
const MIN_LINES: usize = 5;
/// A shared run of this many tokens is one block hit.
const SHINGLE_TOKENS: usize = 12;
/// A run counts toward `block` only if at most this many corpus functions,
/// the unit itself excluded, contain it.
const RARE_MAX_DF: usize = 3;
const FIRE_BLOCK: usize = 6;
const FIRE_COMBO: f64 = 0.5;
/// Matches shown per unit.
const MAX_MATCHES: usize = 3;
/// Units scored per review; the rest are reported `incomplete`.
const MAX_UNITS: usize = 300;
/// Functions whose embed vector is not stored (an incremental refresh drops
/// them until the next full parse) that are encoded in memory per review.
const MAX_ENCODED: usize = 2_000;
const EXCERPT_CHARS: usize = 160;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitKind {
    /// A function the change added.
    Added,
    /// The added lines of a function the change modified.
    Modified,
}

impl UnitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
        }
    }
}

/// An existing function the unit resembles, and how.
#[derive(Debug)]
pub struct Match {
    /// Index into [`DupReport::functions`].
    function: usize,
    /// `(embed + lex) / 2`: the ranking score.
    pub combo: f64,
    pub embed: f64,
    pub lex: f64,
    /// Rare 12-token runs the two share.
    pub shared_runs: usize,
    /// The change added or edited the match too: both sides are new code.
    pub same_change: bool,
    /// The longest token run the two share, so a reader can tell a real copy
    /// from an idiom at a glance.
    pub shared: Option<String>,
}

#[derive(Debug)]
pub struct DupFinding {
    pub kind: UnitKind,
    /// Index into [`DupReport::functions`].
    function: usize,
    /// The declaration line of an added function, the first added line of a
    /// modified one: the firing log's site.
    pub line: i64,
    pub matches: Vec<Match>,
}

#[derive(Default)]
pub struct DupReport {
    /// The index's functions, which findings name by index.
    functions: Vec<CorpusFunction>,
    pub findings: Vec<DupFinding>,
    /// Units scored.
    pub checked: usize,
    /// Why the result may be missing findings. Non-empty means incomplete,
    /// never clean.
    pub incomplete: Vec<String>,
}

impl std::fmt::Debug for DupReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DupReport")
            .field("findings", &self.findings)
            .field("checked", &self.checked)
            .field("incomplete", &self.incomplete)
            .finish()
    }
}

impl DupReport {
    /// The function a finding's unit is in.
    pub fn unit(&self, finding: &DupFinding) -> &CorpusFunction {
        &self.functions[finding.function]
    }

    /// The existing function a match names.
    pub fn matched(&self, m: &Match) -> &CorpusFunction {
        &self.functions[m.function]
    }
}

// ---------------------------------------------------------------- text

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"[A-Za-z_][A-Za-z0-9_]*|\d+|"[^"\n]*"|'[^'\n]*'|\S"#)
        .expect("invariant: the token pattern is valid")
});
static IDENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[A-Za-z_][A-Za-z0-9_]*").expect("invariant: the identifier pattern is valid")
});

/// Keywords and primitive types of the indexed languages: shared by every
/// function, so they say nothing about what a body does.
const KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "pub", "use", "mod", "impl", "self", "Self", "return", "if", "else",
    "match", "for", "in", "while", "loop", "break", "continue", "as", "ref", "struct", "enum",
    "trait", "where", "async", "await", "move", "const", "static", "type", "true", "false", "None",
    "Some", "Ok", "Err", "crate", "super", "dyn", "unsafe", "extern", "final", "var", "void",
    "class", "new", "this", "null", "late", "required", "import", "def", "elif", "pass", "lambda",
    "not", "and", "or", "is", "String", "str", "Vec", "Option", "Result", "usize", "i64", "i32",
    "u8", "u32", "u64", "f64", "bool", "int", "double", "dynamic", "override", "get", "set",
];

/// Drop the signature so a shared name or parameter list does not dominate:
/// the body starts at the first `{` or `=>`. A language with neither reads
/// the body from the parse tree instead ([`TREE_BODY_LANGUAGES`]).
fn body_only(text: &str) -> &str {
    let cut = [text.find('{'), text.find("=>")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(0);
    &text[cut..]
}

/// Languages whose function bodies have no `{` or `=>` for [`body_only`] to
/// cut at: Python's def line, parameters and decorators would stay in the
/// lexical channels (sutra/506).
const TREE_BODY_LANGUAGES: &[&str] = &["python"];

/// Per syntax node with a `body` field: its first and last row and where the
/// body starts, past any docstring (rows 0-based).
type BodySpans = Vec<(usize, usize, Point)>;

/// Where a body's code starts: after its docstring, a leading statement that
/// is only a string. Rust and Dart doc comments sit before the body, so
/// without this Python prose alone could match two functions (sutra/509).
fn code_start(body: tree_sitter::Node<'_>) -> Point {
    let mut cursor = body.walk();
    let mut statements = body.named_children(&mut cursor);
    let Some(first) = statements.next() else {
        return body.start_position();
    };
    let is_docstring = first.kind() == "expression_statement"
        && first.named_child_count() == 1
        && first
            .named_child(0)
            .is_some_and(|s| matches!(s.kind(), "string" | "concatenated_string"));
    if !is_docstring {
        return body.start_position();
    }
    statements
        .next()
        .map_or(first.end_position(), |next| next.start_position())
}

fn body_spans(tree: &Tree) -> BodySpans {
    let mut spans = Vec::new();
    let mut cursor = tree.walk();
    loop {
        let node = cursor.node();
        if let Some(body) = node.child_by_field_name("body") {
            spans.push((
                node.start_position().row,
                node.end_position().row,
                code_start(body),
            ));
        }
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return spans;
            }
        }
    }
}

/// The byte offset in `text`, a function's lines `start..=end` (1-based),
/// where its body starts: the body of the outermost node within the span
/// that ends on its last line, so neither a decorator line nor a nested
/// function's body is taken for it. No such node keeps the text whole.
fn tree_body_start(spans: &[(usize, usize, Point)], text: &str, start: usize, end: usize) -> usize {
    let Some(&(_, _, body)) = spans
        .iter()
        .filter(|(first, last, _)| first + 1 >= start && last + 1 == end)
        .min_by_key(|(first, _, _)| *first)
    else {
        return 0;
    };
    let offset = text
        .split('\n')
        .take(body.row + 1 - start)
        .map(|l| l.len() + 1)
        .sum::<usize>()
        + body.column;
    if text.is_char_boundary(offset) {
        offset
    } else {
        0
    }
}

fn subtokens(text: &str) -> impl Iterator<Item = String> + '_ {
    IDENT
        .find_iter(text)
        .filter(|m| !KEYWORDS.contains(&m.as_str()))
        .flat_map(|m| tokenize(m.as_str()))
}

fn shingle_hash(tokens: &[&str]) -> u64 {
    let mut h = DefaultHasher::new();
    tokens.hash(&mut h);
    h.finish()
}

/// A text's tokens (byte spans) and the hash of every `SHINGLE_TOKENS` run,
/// by starting token.
struct Shingled {
    spans: Vec<Range<usize>>,
    hashes: Vec<u64>,
}

impl Shingled {
    fn new(text: &str) -> Self {
        let spans: Vec<Range<usize>> = TOKEN.find_iter(text).map(|m| m.range()).collect();
        let tokens: Vec<&str> = spans.iter().map(|r| &text[r.start..r.end]).collect();
        let hashes = tokens.windows(SHINGLE_TOKENS).map(shingle_hash).collect();
        Self { spans, hashes }
    }

    /// The distinct hashes, sorted for `binary_search`.
    fn set(&self) -> Vec<u64> {
        let mut set = self.hashes.to_vec();
        set.sort_unstable();
        set.dedup();
        set
    }

    /// The longest run of consecutive shingles `other` also has, as source
    /// text of `text` with whitespace collapsed.
    fn longest_shared(&self, text: &str, other: &[u64]) -> Option<String> {
        let mut best: Option<Range<usize>> = None;
        let mut run_start = None;
        for i in 0..=self.hashes.len() {
            let shared = self
                .hashes
                .get(i)
                .is_some_and(|h| other.binary_search(h).is_ok());
            match (shared, run_start) {
                (true, None) => run_start = Some(i),
                (false, Some(s)) => {
                    if best.as_ref().is_none_or(|b| i - s > b.len()) {
                        best = Some(s..i);
                    }
                    run_start = None;
                }
                _ => {}
            }
        }
        let run = best?;
        let span = self.spans[run.start].start..self.spans[run.end - 1 + SHINGLE_TOKENS - 1].end;
        let collapsed = text[span].split_whitespace().collect::<Vec<_>>().join(" ");
        Some(match collapsed.char_indices().nth(EXCERPT_CHARS) {
            Some((cut, _)) => format!("{}…", &collapsed[..cut]),
            None => collapsed,
        })
    }
}

// ---------------------------------------------------------------- corpus

/// A non-test function of at least `MIN_LINES` lines: a candidate match.
struct Doc<'c> {
    /// Index into the corpus.
    function: usize,
    f: &'c CorpusFunction,
    text: String,
    /// Byte offset of the body in `text`: the signature is dropped from the
    /// lexical channels.
    body: usize,
    /// Sorted, deduplicated shingle hashes of the body.
    shingles: Vec<u64>,
    /// Normalized tf-idf weights by term id.
    lex: Vec<(usize, f64)>,
    embed: Option<HrrVec>,
}

type SymbolKey<'a> = (&'a str, &'a str, &'a str);

impl Doc<'_> {
    fn key(&self) -> SymbolKey<'_> {
        (&self.f.hrr.file_path, &self.f.qualified_name, &self.f.kind)
    }

    fn body(&self) -> &str {
        &self.text[self.body..]
    }

    /// HRR embed cosine; 0 when either side has no vector ([`load_embed`]
    /// reports those).
    fn embed_cosine(&self, other: &Doc<'_>) -> f64 {
        match (&self.embed, &other.embed) {
            (Some(a), Some(b)) => a.dot_product(b),
            _ => 0.0,
        }
    }
}

/// Document frequencies over the corpus: of subtokens (for tf-idf) and of
/// shingles (for rarity).
#[derive(Default)]
struct Lexicon {
    terms: HashMap<String, usize>,
    term_df: Vec<usize>,
    docs: usize,
    /// Per term id, the documents containing it and their weights.
    postings: Vec<Vec<(usize, f64)>>,
    /// Per shingle: how many documents contain it, and the first
    /// `RARE_MAX_DF + 1` of them (enough for every rare shingle).
    shingles: HashMap<u64, (usize, [usize; RARE_MAX_DF + 1])>,
}

impl Lexicon {
    /// Count the terms of a document's body.
    fn count(&mut self, body: &str) -> HashMap<usize, u32> {
        let mut counts = HashMap::new();
        for t in subtokens(body) {
            let next = self.terms.len();
            let id = *self.terms.entry(t).or_insert(next);
            if id == self.term_df.len() {
                self.term_df.push(0);
            }
            *counts.entry(id).or_insert(0) += 1;
        }
        for id in counts.keys() {
            self.term_df[*id] += 1;
        }
        counts
    }

    fn add_shingles(&mut self, doc: usize, shingles: &[u64]) {
        for h in shingles {
            let entry = self.shingles.entry(*h).or_insert((0, [0; RARE_MAX_DF + 1]));
            if entry.0 <= RARE_MAX_DF {
                entry.1[entry.0] = doc;
            }
            entry.0 += 1;
        }
    }

    fn weight(&self, df: usize, count: u32) -> f64 {
        (1.0 + f64::from(count).ln()) * ((self.docs + 1) as f64 / (df + 1) as f64).ln()
    }

    fn normalize(mut v: Vec<(usize, f64)>, extra_sq: f64) -> Vec<(usize, f64)> {
        let norm = (v.iter().map(|(_, w)| w * w).sum::<f64>() + extra_sq).sqrt();
        if norm > 0.0 {
            v.iter_mut().for_each(|(_, w)| *w /= norm);
        }
        v
    }

    fn doc_vector(&self, counts: &HashMap<usize, u32>) -> Vec<(usize, f64)> {
        let v = counts
            .iter()
            .map(|(&id, &c)| (id, self.weight(self.term_df[id], c)))
            .collect();
        Self::normalize(v, 0.0)
    }

    /// The tf-idf vector of an arbitrary text. Terms no document has still
    /// count toward its norm.
    fn query_vector(&self, text: &str) -> Vec<(usize, f64)> {
        let mut known: HashMap<usize, u32> = HashMap::new();
        let mut unknown: HashMap<String, u32> = HashMap::new();
        for t in subtokens(text) {
            match self.terms.get(&t) {
                Some(&id) => *known.entry(id).or_insert(0) += 1,
                None => *unknown.entry(t).or_insert(0) += 1,
            }
        }
        let extra: f64 = unknown.values().map(|&c| self.weight(0, c).powi(2)).sum();
        let v = known
            .iter()
            .map(|(&id, &c)| (id, self.weight(self.term_df[id], c)))
            .collect();
        Self::normalize(v, extra)
    }

    /// Per document, its tf-idf cosine with `query`.
    fn lex_scores(&self, query: &[(usize, f64)], docs: usize) -> Vec<f64> {
        let mut lex = vec![0.0f64; docs];
        for &(id, w) in query {
            for &(d, dw) in &self.postings[id] {
                lex[d] += w * dw;
            }
        }
        lex
    }

    /// Per document, the rare shingles of `query` it shares. A shingle is
    /// rare when at most `RARE_MAX_DF` documents other than `own` hold it.
    fn block_scores(&self, query: &[u64], own: &[u64], docs: usize) -> Vec<usize> {
        let mut block = vec![0usize; docs];
        for h in query {
            let Some((df, first)) = self.shingles.get(h) else {
                continue;
            };
            let others = df - usize::from(own.binary_search(h).is_ok());
            if others <= RARE_MAX_DF {
                for &d in &first[..(*df).min(RARE_MAX_DF + 1)] {
                    block[d] += 1;
                }
            }
        }
        block
    }
}

/// Lines `refs` in `lines` call `target`: a resolved reference to it, or an
/// unresolved one by its name whose qualifier names its type or module. A
/// bare short name is not enough (both `build`s in ai/197 are `build`). Only
/// a call counts: naming the function as a value or type is not delegating
/// to it (sutra/505).
fn calls(refs: &[RefRow], lines: &impl Fn(usize) -> bool, target: &CorpusFunction) -> bool {
    refs.iter().any(|r| {
        r.context_kind == RefContextKind::Call.as_str()
            && usize::try_from(r.line).is_ok_and(lines)
            && match r.target_symbol_id {
                Some(id) => id == target.hrr.symbol_id,
                None => {
                    r.unresolved_name.as_deref() == Some(target.short_name.as_str())
                        && r.qualifier.as_deref().is_some_and(|q| {
                            qualifier_fits(
                                Some(q),
                                (&target.qualified_name, &target.hrr.file_path),
                                false,
                            )
                        })
                }
            }
    })
}

// ---------------------------------------------------------------- units

/// Code the change added: a whole function, or a modified function's added
/// lines.
struct Unit {
    doc: usize,
    kind: UnitKind,
    /// What is scored: the body, or the added lines.
    text: String,
    /// The unit's lines, for `delegates`.
    lines: BTreeSet<usize>,
    site_line: i64,
}

type OwnedKey = (String, String, String);

/// What the diff says about the changed files.
#[derive(Default)]
struct DiffUnits {
    /// `(file, qualified name, kind)` of functions the change added.
    added: Vec<OwnedKey>,
    /// `(file, qualified name, kind)` of modified functions, with their
    /// pre-change source.
    modified: Vec<(OwnedKey, String)>,
    /// Added line ranges of each new-side file.
    added_lines: HashMap<String, Vec<Range<usize>>>,
    /// Files the reviewed side deleted. The index may still hold them (a
    /// staged deletion recreated in the worktree), so their functions are
    /// not matches (sutra/506).
    gone: HashSet<String>,
}

fn borrowed(key: &OwnedKey) -> SymbolKey<'_> {
    (&key.0, &key.1, &key.2)
}

fn span_text(source: &str, start_line: usize, end_line: usize) -> String {
    let start = start_line.max(1);
    source
        .lines()
        .skip(start - 1)
        .take((end_line + 1).saturating_sub(start))
        .collect::<Vec<_>>()
        .join("\n")
}

fn diff_units(
    db: &Db,
    workspace_root: &Path,
    scope: &DiffScope,
    registry: &LanguageRegistry,
    incomplete: &mut Vec<String>,
) -> Result<DiffUnits> {
    let file_hunks = git::git_diff_hunks(
        workspace_root,
        &scope.base_revision,
        scope.head_revision.as_deref(),
    )?;
    let mut pool = ParserPool::new(std::time::Duration::from_secs(5));
    let mut units = DiffUnits::default();
    let (mut unmatched_old, mut unmatched_new): (Vec<UnmatchedSymbol>, Vec<UnmatchedSymbol>) =
        (Vec::new(), Vec::new());
    for fh in &file_hunks {
        let Some(path) = fh.new_path.as_deref().or(fh.old_path.as_deref()) else {
            continue;
        };
        let Some(adapter) = registry.adapter_for_path(path) else {
            continue;
        };
        if !db.indexes_language(adapter.language_id())? {
            continue;
        }
        if let Some(new_path) = fh.new_path.as_deref() {
            if let Some(why) = index_mismatch(db, workspace_root, scope, new_path) {
                incomplete.push(why);
            }
            units.added_lines.insert(
                new_path.to_string(),
                fh.hunks.iter().map(git::Hunk::added_lines).collect(),
            );
        }
        let (old_src, new_src) = match read_sides(workspace_root, scope, fh) {
            Ok(sides) => sides,
            Err(e) => {
                incomplete.push(format!("{path}: {e}"));
                continue;
            }
        };
        let old_path = fh.old_path.as_deref().unwrap_or(path);
        let new_path = fh.new_path.as_deref().unwrap_or(path);
        // A side with syntax errors yields partial symbols, so classifying
        // against it would invent or hide units: the file is not checked.
        let mut parse = |src: Option<&str>, p: &str| -> Option<ParseResult> {
            match pool.parse_with(adapter, src?, p) {
                Ok(parse) if parse.parsed_ok => Some(parse),
                Ok(_) => {
                    incomplete.push(format!(
                        "{p}: syntax errors, so its changed functions were not checked"
                    ));
                    None
                }
                Err(e) => {
                    incomplete.push(format!("{p}: {e}"));
                    None
                }
            }
        };
        let old = parse(old_src.as_deref(), old_path);
        let new = parse(new_src.as_deref(), new_path);
        match (&old_src, &new_src, &old, &new) {
            (Some(o), Some(n), Some(op), Some(np)) => {
                let result =
                    classify_symbols(op, np, (o, n), (old_path, new_path), ContainerScope::Whole);
                let old_flat = flatten_symbols(&op.symbols);
                for change in result.changes {
                    if !matches!(
                        change.change,
                        ChangeKind::BodyChanged | ChangeKind::SignatureChanged
                    ) {
                        continue;
                    }
                    let Some(before) = old_flat.iter().find(|s| {
                        s.qualified_name == change.symbol && s.kind.as_str() == change.kind
                    }) else {
                        continue;
                    };
                    let body = span_text(o, before.start_line, before.end_line);
                    units
                        .modified
                        .push(((new_path.to_string(), change.symbol, change.kind), body));
                }
                unmatched_old.extend(result.unmatched_old);
                unmatched_new.extend(result.unmatched_new);
            }
            (Some(o), None, Some(op), _) => unmatched_old.extend(build_unmatched(op, o, old_path)),
            (None, Some(n), _, Some(np)) => unmatched_new.extend(build_unmatched(np, n, new_path)),
            _ => {}
        }
    }
    let new_paths: HashSet<&str> = file_hunks
        .iter()
        .filter_map(|fh| fh.new_path.as_deref())
        .collect();
    units.gone = file_hunks
        .iter()
        .filter_map(|fh| fh.old_path.as_deref())
        .filter(|p| !new_paths.contains(p))
        .map(str::to_string)
        .collect();
    // Renamed or moved functions are not new code.
    let moved = resolve_renames(&unmatched_old, &unmatched_new).matched_new;
    units.added = unmatched_new
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !moved.contains(i))
        .map(|(_, s)| (s.file, s.qualified_name, s.kind))
        .collect();
    Ok(units)
}

/// The units among `docs`, in file and line order, and the pre-change
/// source of each modified document.
fn collect_units<'d>(
    docs: &[Doc<'_>],
    diff: &'d DiffUnits,
) -> (Vec<Unit>, HashMap<usize, &'d str>) {
    let mut by_key: HashMap<SymbolKey<'_>, Vec<usize>> = HashMap::new();
    for (i, doc) in docs.iter().enumerate() {
        by_key.entry(doc.key()).or_default().push(i);
    }
    let mut units = Vec::new();
    for key in &diff.added {
        for &i in by_key.get(&borrowed(key)).into_iter().flatten() {
            let f = docs[i].f;
            units.push(Unit {
                doc: i,
                kind: UnitKind::Added,
                text: docs[i].body().to_string(),
                lines: (to_line(f.hrr.start_line)..=to_line(f.hrr.end_line)).collect(),
                site_line: f.hrr.start_line,
            });
        }
    }
    let mut before: HashMap<usize, &str> = HashMap::new();
    for (key, old) in &diff.modified {
        for &i in by_key.get(&borrowed(key)).into_iter().flatten() {
            before.insert(i, old);
            let f = docs[i].f;
            let (start, end) = (to_line(f.hrr.start_line), to_line(f.hrr.end_line));
            let lines: BTreeSet<usize> = diff
                .added_lines
                .get(&f.hrr.file_path)
                .into_iter()
                .flatten()
                .flat_map(|r| r.start.max(start)..r.end.min(end + 1))
                .collect();
            let text_lines: Vec<&str> = docs[i].text.lines().collect();
            let added: Vec<&str> = lines
                .iter()
                .filter_map(|l| text_lines.get(l - start).copied())
                .collect();
            if added.iter().filter(|l| !l.trim().is_empty()).count() < MIN_LINES {
                continue;
            }
            let Some(&first) = lines.first() else {
                continue;
            };
            units.push(Unit {
                doc: i,
                kind: UnitKind::Modified,
                text: added.join("\n"),
                site_line: i64::try_from(first).expect("invariant: a line number fits i64"),
                lines,
            });
        }
    }
    units.sort_by(|a, b| {
        docs[a.doc]
            .key()
            .cmp(&docs[b.doc].key())
            .then(a.site_line.cmp(&b.site_line))
    });
    (units, before)
}

// ---------------------------------------------------------------- analysis

/// Find the dup-exists matches of `scope`. [`run_advisory`] first checks
/// that the index can stand for the reviewed side.
pub fn analyze(
    db: &Db,
    workspace_root: &Path,
    scope: &DiffScope,
    registry: &LanguageRegistry,
) -> Result<DupReport> {
    let mut report = DupReport::default();
    report
        .incomplete
        .extend(dirty_outside_diff(db, workspace_root, scope, registry)?);
    let diff = diff_units(db, workspace_root, scope, registry, &mut report.incomplete)?;
    if diff.added.is_empty() && diff.modified.is_empty() {
        return Ok(report);
    }
    let corpus = db.function_corpus()?;
    report.findings = score(db, workspace_root, registry, &corpus, &diff, &mut report)?;
    report.functions = corpus;
    Ok(report)
}

/// Build the corpus documents and score every unit against them.
fn score(
    db: &Db,
    workspace_root: &Path,
    registry: &LanguageRegistry,
    corpus: &[CorpusFunction],
    diff: &DiffUnits,
    report: &mut DupReport,
) -> Result<Vec<DupFinding>> {
    let Corpus {
        mut docs, lexicon, ..
    } = Corpus::build(
        workspace_root,
        registry,
        corpus,
        |f| !diff.gone.contains(&f.hrr.file_path),
        None,
        &mut report.incomplete,
    );

    let (mut units, before) = collect_units(&docs, diff);
    if units.len() > MAX_UNITS {
        let skipped: Vec<&str> = units[MAX_UNITS..]
            .iter()
            .map(|u| docs[u.doc].f.qualified_name.as_str())
            .collect();
        report.incomplete.push(format!(
            "unit cap ({MAX_UNITS}): {} changed function(s) not checked: {}",
            skipped.len(),
            list_some(&skipped)
        ));
        units.truncate(MAX_UNITS);
    }
    report.checked = units.len();
    load_embed(
        db,
        workspace_root,
        &mut docs,
        Encode {
            first: |_, d: &Doc<'_>| !diff.added_lines.contains_key(&d.f.hrr.file_path),
            order: "the changed files first, then the rest in index order",
            cap: MAX_ENCODED,
        },
        &mut report.incomplete,
    )?;

    let is_unit: HashSet<usize> = units.iter().map(|u| u.doc).collect();
    let added: HashSet<usize> = units
        .iter()
        .filter(|u| u.kind == UnitKind::Added)
        .map(|u| u.doc)
        .collect();
    let mut refs: HashMap<i64, Vec<RefRow>> = HashMap::new();
    let mut emitted: HashSet<(usize, usize)> = HashSet::new();
    let mut findings = Vec::new();
    for unit in &units {
        let q = &docs[unit.doc];
        let shingled = Shingled::new(&unit.text);
        let q_set = shingled.set();
        // A modified function's runs count only if the change wrote them:
        // re-indenting or moving a block it already held is not a copy.
        let new_runs: Vec<u64> = match (unit.kind, before.get(&unit.doc)) {
            (UnitKind::Modified, Some(old)) => {
                let old = Shingled::new(old).set();
                q_set
                    .iter()
                    .copied()
                    .filter(|h| old.binary_search(h).is_err())
                    .collect()
            }
            _ => q_set.to_vec(),
        };
        let block = lexicon.block_scores(&new_runs, &q.shingles, docs.len());
        let modified_query;
        let query: &[(usize, f64)] = match unit.kind {
            UnitKind::Added => &q.lex,
            UnitKind::Modified => {
                modified_query = lexicon.query_vector(&unit.text);
                &modified_query
            }
        };
        let lex = lexicon.lex_scores(query, docs.len());

        let mut candidates: Vec<(usize, f64, f64)> = Vec::new();
        for (d, embed, combo) in rivals(&docs, q, &lex) {
            // embed sees a modified function whole, so its combo speaks for
            // the function's standing family, not the lines the change added.
            let combo_fires = unit.kind == UnitKind::Added && combo >= FIRE_COMBO;
            if block[d] >= FIRE_BLOCK || combo_fires {
                candidates.push((d, combo, embed));
            }
        }
        candidates.sort_by(|a, b| b.1.total_cmp(&a.1).then(block[b.0].cmp(&block[a.0])));

        let mut matches = Vec::new();
        for (d, combo, embed) in candidates {
            if matches.len() == MAX_MATCHES {
                break;
            }
            // Two copies the change added: report the pair once.
            if is_unit.contains(&d) && emitted.contains(&(d, unit.doc)) {
                continue;
            }
            let m = &docs[d];
            let unit_refs = file_refs(db, &mut refs, q.f.hrr.file_id)?;
            if calls(unit_refs, &|l| unit.lines.contains(&l), m.f) {
                continue; // delegates
            }
            if let Some(old) = before.get(&d) {
                let span = to_line(m.f.hrr.start_line)..=to_line(m.f.hrr.end_line);
                let shared = |set: &[u64]| {
                    q_set
                        .iter()
                        .filter(|h| set.binary_search(h).is_ok())
                        .count()
                };
                let (was, now) = (shared(&Shingled::new(old).set()), shared(&m.shingles));
                let match_refs = file_refs(db, &mut refs, m.f.hrr.file_id)?;
                if calls(match_refs, &|l| span.contains(&l), q.f) || (was >= 4 && now * 2 < was) {
                    continue; // extracted
                }
            }
            emitted.insert((unit.doc, d));
            matches.push(Match {
                function: m.function,
                combo: round3(combo),
                embed: round3(embed),
                lex: round3(lex[d]),
                shared_runs: block[d],
                same_change: added.contains(&d) || before.contains_key(&d),
                shared: shingled.longest_shared(&unit.text, &m.shingles),
            });
        }
        if !matches.is_empty() {
            findings.push(DupFinding {
                kind: unit.kind,
                function: q.function,
                line: unit.site_line,
                matches,
            });
        }
    }
    Ok(findings)
}

/// Every other same-language document `q` could duplicate, with its embed
/// cosine and combo `(embed + lex) / 2`, the ranking score. `lex` is `q`'s
/// [`Lexicon::lex_scores`].
fn rivals<'d>(
    docs: &'d [Doc<'_>],
    q: &'d Doc<'_>,
    lex: &'d [f64],
) -> impl Iterator<Item = (usize, f64, f64)> + 'd {
    docs.iter().enumerate().filter_map(move |(d, doc)| {
        if doc.key() == q.key() || doc.f.hrr.language != q.f.hrr.language {
            return None;
        }
        let embed = q.embed_cosine(doc);
        Some((d, embed, (embed + lex[d]) / 2.0))
    })
}

/// The candidate matches and their document frequencies: what both the
/// advisory and `sutra_similar` score against (sutra/484).
struct Corpus<'c> {
    docs: Vec<Doc<'c>>,
    lexicon: Lexicon,
    /// `always` passed the filters every other function had to: the
    /// advisory would check it as a unit (sutra/511).
    always_eligible: bool,
}

impl<'c> Corpus<'c> {
    /// Every non-test function of at least `MIN_LINES` lines that `keep`
    /// admits, plus `always` whatever it is, with its lexical and block
    /// channels. Embed vectors are left to [`load_embed`].
    fn build(
        workspace_root: &Path,
        registry: &LanguageRegistry,
        corpus: &'c [CorpusFunction],
        keep: impl Fn(&CorpusFunction) -> bool,
        always: Option<i64>,
        incomplete: &mut Vec<String>,
    ) -> Self {
        let is_test = |f: &CorpusFunction| {
            parser::flags_mark_test(f.flags, &f.hrr.language)
                || registry
                    .adapter_for_path(&f.hrr.file_path)
                    .is_some_and(|a| a.is_test_path(&f.hrr.file_path))
        };
        let mut pool = ParserPool::new(std::time::Duration::from_secs(5));
        let mut sources: HashMap<&str, Option<(String, Option<BodySpans>)>> = HashMap::new();
        let mut lexicon = Lexicon::default();
        let mut docs: Vec<Doc<'_>> = Vec::new();
        let mut counts: Vec<HashMap<usize, u32>> = Vec::new();
        let mut always_eligible = false;
        for (function, f) in corpus.iter().enumerate() {
            let (start, end) = (to_line(f.hrr.start_line), to_line(f.hrr.end_line));
            let small = end + 1 - start < MIN_LINES;
            let eligible = !small && !is_test(f) && keep(f);
            if always == Some(f.hrr.symbol_id) {
                always_eligible = eligible;
            } else if !eligible {
                continue;
            }
            let path = f.hrr.file_path.as_str();
            let source = sources.entry(path).or_insert_with(|| {
                match std::fs::read_to_string(workspace_root.join(path)) {
                    Ok(s) => {
                        let adapter = registry.adapter_for_path(path)
                            .filter(|a| TREE_BODY_LANGUAGES.contains(&a.language_id()));
                        let spans = match adapter.map(|a| pool.tree(a, &s)) {
                            Some(Ok(tree)) => Some(body_spans(&tree)),
                            // The signature stays in its functions' lexical
                            // channels, which dilutes their scores.
                            Some(Err(e)) => {
                                incomplete.push(format!(
                                    "{path}: {e}, so its functions were scored with their signatures"
                                ));
                                None
                            }
                            None => None,
                        };
                        Some((s, spans))
                    }
                    Err(e) => {
                        incomplete.push(format!("{path}: {e}"));
                        None
                    }
                }
            });
            let Some((source, spans)) = source else {
                continue;
            };
            let text = span_text(source, start, end);
            let body = match spans {
                Some(spans) => tree_body_start(spans, &text, start, end),
                None => text.len() - body_only(&text).len(),
            };
            let shingles = Shingled::new(&text[body..]).set();
            lexicon.add_shingles(docs.len(), &shingles);
            counts.push(lexicon.count(&text[body..]));
            docs.push(Doc {
                function,
                f,
                text,
                body,
                shingles,
                lex: Vec::new(),
                embed: None,
            });
        }
        lexicon.docs = docs.len();
        lexicon.postings = vec![Vec::new(); lexicon.terms.len()];
        for (i, (doc, c)) in docs.iter_mut().zip(&counts).enumerate() {
            doc.lex = lexicon.doc_vector(c);
            for &(id, w) in &doc.lex {
                lexicon.postings[id].push((i, w));
            }
        }
        Self {
            docs,
            lexicon,
            always_eligible,
        }
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

fn to_line(line: i64) -> usize {
    usize::try_from(line).expect("invariant: a source line number is positive")
}

fn file_refs<'r>(
    db: &Db,
    cache: &'r mut HashMap<i64, Vec<RefRow>>,
    file_id: i64,
) -> Result<&'r [RefRow]> {
    if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(file_id) {
        e.insert(db.find_refs_in_file(file_id)?);
    }
    Ok(&cache[&file_id])
}

fn list_some(names: &[&str]) -> String {
    const SHOWN: usize = 5;
    let mut out = names
        .iter()
        .take(SHOWN)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        out.push_str(&format!(" (+{} more)", names.len() - SHOWN));
    }
    out
}

/// Which documents without a stored embed vector [`load_embed`] encodes:
/// up to `cap`, ascending `first`, ties in corpus order. `order` says how,
/// for the `incomplete` note on the ones past the cap.
struct Encode<F> {
    first: F,
    order: &'static str,
    cap: usize,
}

/// Attach each document's embed vector, normalized: stored, or encoded in
/// memory up to `encode.cap`, in `encode.first` order. Vectors are missing when an
/// incremental refresh dropped them, and all of them in a strip-only
/// workspace; under `SUTRA_SIMILARITY_MODE=off` none are encoded. A function
/// still without one scores embed 0, which halves its combo, so every function
/// left without one — past the cap, too long to encode, or failed to encode —
/// is reported.
fn load_embed<K: Ord>(
    db: &Db,
    workspace_root: &Path,
    docs: &mut [Doc<'_>],
    encode: Encode<impl Fn(usize, &Doc<'_>) -> K>,
    incomplete: &mut Vec<String>,
) -> Result<()> {
    let mut stored: HashMap<i64, HrrVec> =
        db.load_all_vectors_by_mode("embed")?.into_iter().collect();
    let (mut missing, too_long): (Vec<usize>, Vec<usize>) = (0..docs.len())
        .filter(|&i| !stored.contains_key(&docs[i].f.hrr.symbol_id))
        .partition(|&i| {
            let f = docs[i].f;
            f.hrr.end_line - f.hrr.start_line <= MAX_HRR_SYMBOL_LINES
        });
    let (mode, _) = crate::similarity::resolve_similarity_mode(db)?;
    let cap = if mode == SimilarityMode::Off {
        0
    } else {
        encode.cap
    };
    missing.sort_by_cached_key(|&i| (encode.first)(i, &docs[i]));
    let over = missing.len().saturating_sub(cap);
    missing.truncate(cap);
    let rows: Vec<_> = missing.iter().map(|&i| &docs[i].f.hrr).collect();
    let encoded = crate::similarity::encode_embed_vectors(workspace_root, &rows)?;
    let failed: Vec<&str> = missing
        .iter()
        .filter(|&&i| !encoded.contains_key(&docs[i].f.hrr.symbol_id))
        .map(|&i| docs[i].f.qualified_name.as_str())
        .collect();
    stored.extend(encoded);
    if over > 0 {
        let why = match mode {
            SimilarityMode::Off => "SUTRA_SIMILARITY_MODE=off encodes none".to_string(),
            _ => format!("only {cap} are encoded per request, {}", encode.order),
        };
        let fix = match mode {
            SimilarityMode::Full => "a full `sutra parse` stores them",
            SimilarityMode::StripOnly => {
                "the workspace is strip-only, which stores none: SUTRA_SIMILARITY_MODE=full and \
                 a full `sutra parse` store them"
            }
            SimilarityMode::Off => "SUTRA_SIMILARITY_MODE=full and a full `sutra parse` store them",
        };
        incomplete.push(format!(
            "embed: {over} function(s) have no stored embed vector and were not encoded ({why}), \
             so every embed score against them reads 0 ({fix})"
        ));
    }
    if !too_long.is_empty() {
        let names: Vec<&str> = too_long
            .iter()
            .map(|&i| docs[i].f.qualified_name.as_str())
            .collect();
        incomplete.push(format!(
            "embed line cap ({MAX_HRR_SYMBOL_LINES}): {} function(s) are too long to embed, so \
             their embed score reads 0: {}",
            names.len(),
            list_some(&names)
        ));
    }
    if !failed.is_empty() {
        incomplete.push(format!(
            "embed encode: {} function(s) could not be encoded (file unreadable or \
             unparseable), so their embed score reads 0: {}",
            failed.len(),
            list_some(&failed)
        ));
    }
    for doc in docs {
        doc.embed = stored.remove(&doc.f.hrr.symbol_id).map(|v| v.normalize());
    }
    Ok(())
}

// ---------------------------------------------------------------- similar

/// The functions most likely to duplicate one function: `sutra_similar`'s
/// default ranking (sutra/484). The advisory's channels, with the whole
/// function as the query. There is no change to read, so nothing is
/// suppressed as `delegates` or `extracted`.
pub struct Neighbours {
    functions: Vec<CorpusFunction>,
    query: usize,
    /// The advisory would check the query at all: a non-test function of at
    /// least `MIN_LINES` lines. Otherwise no match would fire (sutra/511).
    query_checked: bool,
    /// Best first: every match that would fire in the advisory (`block >= 6`
    /// or `combo >= 0.5`), then the rest by combo.
    pub matches: Vec<Match>,
    /// Same-language functions scored against.
    pub candidates: usize,
    /// Why the ranking may be missing matches. Non-empty means incomplete.
    pub incomplete: Vec<String>,
}

impl Neighbours {
    pub fn query(&self) -> &CorpusFunction {
        &self.functions[self.query]
    }

    pub fn matched(&self, m: &Match) -> &CorpusFunction {
        &self.functions[m.function]
    }

    /// The match would fire in the advisory: it would check the query, and
    /// the match clears a firing bar.
    pub fn likely_duplicate(&self, m: &Match) -> bool {
        self.query_checked && fires(m)
    }
}

fn fires(m: &Match) -> bool {
    m.shared_runs >= FIRE_BLOCK || m.combo >= FIRE_COMBO
}

/// Rank the functions `symbol_id` may duplicate. Keeps every match that
/// would fire, whatever its combo, and the rest down to `threshold` combo,
/// up to `limit`: `threshold` filters only the matches the advisory would
/// not report (sutra/511).
/// `None` when `symbol_id` is not an indexed function.
pub fn neighbours(
    db: &Db,
    workspace_root: &Path,
    registry: &LanguageRegistry,
    symbol_id: i64,
    limit: usize,
    threshold: f64,
) -> Result<Option<Neighbours>> {
    rank_neighbours(
        db,
        workspace_root,
        registry,
        symbol_id,
        limit,
        threshold,
        MAX_ENCODED,
    )
}

/// [`neighbours`], encoding at most `encode_cap` missing embed vectors.
fn rank_neighbours(
    db: &Db,
    workspace_root: &Path,
    registry: &LanguageRegistry,
    symbol_id: i64,
    limit: usize,
    threshold: f64,
    encode_cap: usize,
) -> Result<Option<Neighbours>> {
    let functions = db.function_corpus()?;
    let Some(query) = functions.iter().position(|f| f.hrr.symbol_id == symbol_id) else {
        return Ok(None);
    };
    let mut incomplete = Vec::new();
    let Corpus {
        mut docs,
        lexicon,
        always_eligible,
    } = Corpus::build(
        workspace_root,
        registry,
        &functions,
        |_| true,
        Some(symbol_id),
        &mut incomplete,
    );
    let mut matches = Vec::new();
    let mut candidates = 0;
    // Absent only when its file could not be read, which `incomplete` says.
    if let Some(q) = docs.iter().position(|d| d.function == query) {
        let (block, lex) = {
            let q = &docs[q];
            (
                lexicon.block_scores(&q.shingles, &q.shingles, docs.len()),
                lexicon.lex_scores(&q.lex, docs.len()),
            )
        };
        // Missing embed vectors go to the query, then the same-language
        // functions closest on the other two channels: past the cap, embed 0
        // falls on the ones least likely to rank (sutra/510).
        let language = docs[q].f.hrr.language.as_str();
        let mut closest: Vec<usize> = (0..docs.len()).collect();
        closest.sort_by(|&a, &b| block[b].cmp(&block[a]).then(lex[b].total_cmp(&lex[a])));
        let mut rank = vec![0; docs.len()];
        for (r, &d) in closest.iter().enumerate() {
            rank[d] = r;
        }
        load_embed(
            db,
            workspace_root,
            &mut docs,
            Encode {
                first: |d, doc: &Doc<'_>| (d != q, doc.f.hrr.language != language, rank[d]),
                order: "the query first, then the functions closest on shared runs and \
                        identifiers",
                cap: encode_cap,
            },
            &mut incomplete,
        )?;
        let q = &docs[q];
        let mut ranked: Vec<(bool, usize, Match)> = Vec::new();
        for (d, embed, combo) in rivals(&docs, q, &lex) {
            candidates += 1;
            let m = Match {
                function: docs[d].function,
                combo,
                embed,
                lex: lex[d],
                shared_runs: block[d],
                same_change: false,
                shared: None,
            };
            let fired = always_eligible && fires(&m);
            if fired || m.combo >= threshold {
                ranked.push((fired, d, m));
            }
        }
        ranked.sort_by(|(fa, _, a), (fb, _, b)| {
            fb.cmp(fa)
                .then(b.combo.total_cmp(&a.combo))
                .then(b.shared_runs.cmp(&a.shared_runs))
        });
        ranked.truncate(limit);
        let shingled = Shingled::new(q.body());
        matches = ranked
            .into_iter()
            .map(|(_, d, mut m)| {
                m.shared = shingled.longest_shared(q.body(), &docs[d].shingles);
                m.combo = round3(m.combo);
                m.embed = round3(m.embed);
                m.lex = round3(m.lex);
                m
            })
            .collect();
    }
    Ok(Some(Neighbours {
        functions,
        query,
        query_checked: always_eligible,
        matches,
        candidates,
        incomplete,
    }))
}

// ---------------------------------------------------------------- surface

/// The advisory as a review surface reports it: findings, or why there are
/// none. Shared by `sutra_review` and `sutra check`.
#[derive(Debug, Default)]
pub struct Advisory {
    pub report: DupReport,
    /// The check did not run, and why: the index does not hold the reviewed side.
    pub skipped: Option<String>,
    /// The check itself failed; `report` is empty and must not read as clean.
    pub error: Option<String>,
    /// The findings stand, but recording them in the firing log failed.
    pub firing_log_error: Option<String>,
}

/// One `(unit, match)` pair.
pub type Pair<'a> = (&'a DupFinding, &'a Match);

impl Advisory {
    /// Pairs grouped by (unit file, matched file): a new adapter mirroring an
    /// old one is one group, not seventeen items. Strongest first.
    pub fn groups(&self) -> BTreeMap<(&str, &str), Vec<Pair<'_>>> {
        let report = &self.report;
        let mut groups: BTreeMap<(&str, &str), Vec<Pair<'_>>> = BTreeMap::new();
        for f in &report.findings {
            for m in &f.matches {
                let key = (
                    report.unit(f).hrr.file_path.as_str(),
                    report.matched(m).hrr.file_path.as_str(),
                );
                groups.entry(key).or_default().push((f, m));
            }
        }
        for pairs in groups.values_mut() {
            pairs.sort_by(|a, b| {
                b.1.combo
                    .total_cmp(&a.1.combo)
                    .then(b.1.shared_runs.cmp(&a.1.shared_runs))
            });
        }
        groups
    }

    pub fn to_json(&self) -> serde_json::Value {
        let report = &self.report;
        let groups: Vec<_> = self
            .groups()
            .into_iter()
            .map(|((file, matched_file), pairs)| {
                json!({
                    "file": file,
                    "matched_file": matched_file,
                    "pairs": pairs.iter().map(|(f, m)| {
                        let mut pair = json!({
                            "symbol": report.unit(f).qualified_name,
                            "kind": f.kind,
                            "line": f.line,
                            "matches": report.matched(m).qualified_name,
                            "match_line": report.matched(m).hrr.start_line,
                            "combo": m.combo,
                            "embed": m.embed,
                            "lex": m.lex,
                            "shared_runs": m.shared_runs,
                            "same_change": m.same_change,
                        });
                        if let Some(s) = &m.shared {
                            pair["shared"] = json!(s);
                        }
                        pair
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut out = json!({
            "advisory": true,
            "detail": "code like this already exists: each pair names the existing function and what the two share",
            "checked": report.checked,
            "groups": groups,
        });
        if !report.incomplete.is_empty() {
            out["incomplete"] = json!(report.incomplete);
        }
        if let Some(s) = &self.skipped {
            out["skipped"] = json!(s);
        }
        if let Some(e) = &self.error {
            out["error"] = json!(e);
        }
        if let Some(e) = &self.firing_log_error {
            out["firing_log_error"] = json!(e);
        }
        out
    }

    /// Human-readable lines for `sutra check`.
    pub fn render(&self, out: &mut String) {
        use std::fmt::Write;
        const SHOWN: usize = 3;
        let report = &self.report;
        if let Some(e) = &self.error {
            let _ = writeln!(out, "\ndup-exists check failed (not gating): {e}");
        }
        if let Some(s) = &self.skipped {
            let _ = writeln!(out, "\ndup-exists check skipped: {s}");
        }
        let groups = self.groups();
        if !groups.is_empty() {
            let _ = writeln!(
                out,
                "\ncode like this already exists, in {} file pair(s) (advisory, not gating):",
                groups.len()
            );
        }
        for ((file, matched_file), pairs) in &groups {
            let _ = writeln!(out, "  {file} ~ {matched_file} ({} pair(s))", pairs.len());
            for (f, m) in pairs.iter().take(SHOWN) {
                let _ = writeln!(
                    out,
                    "    [{}] {}:{} ~ {}:{}  combo {:.2}, {} shared run(s)",
                    f.kind.as_str(),
                    report.unit(f).qualified_name,
                    f.line,
                    report.matched(m).qualified_name,
                    report.matched(m).hrr.start_line,
                    m.combo,
                    m.shared_runs
                );
                if let Some(s) = &m.shared {
                    let _ = writeln!(out, "      shared: {s}");
                }
            }
            if pairs.len() > SHOWN {
                let _ = writeln!(out, "    (+{} more)", pairs.len() - SHOWN);
            }
        }
        if !report.incomplete.is_empty() {
            let _ = writeln!(
                out,
                "  dup-exists check incomplete: {}",
                report.incomplete.join("; ")
            );
        }
        if let Some(e) = &self.firing_log_error {
            let _ = writeln!(out, "  (firing log not written: {e})");
        }
    }
}

/// Run the check on `scope` and log what it flagged under the review event
/// `patch` identifies. Never fails the caller: a failure is carried in the
/// result so the surface can say so.
pub fn run_advisory(
    db: &Db,
    workspace_root: &Path,
    scope: &DiffScope,
    registry: &LanguageRegistry,
    at: (&str, &str),
    patch: std::result::Result<&ReviewedPatch, &str>,
) -> Advisory {
    if let Some(skipped) = advisory::index_cannot_hold(workspace_root, scope) {
        return Advisory {
            skipped: Some(skipped),
            ..Advisory::default()
        };
    }
    match analyze(db, workspace_root, scope, registry) {
        Ok(report) => {
            let firing_log_error = match patch {
                _ if report.findings.is_empty() => None,
                Ok(patch) => record_firings(db, workspace_root, &report, at, scope, patch)
                    .err()
                    .map(|e| e.to_string()),
                Err(e) => Some(format!(
                    "no review event: the diff could not be hashed: {e}"
                )),
            };
            Advisory {
                report,
                firing_log_error,
                ..Advisory::default()
            }
        }
        Err(e) => Advisory {
            error: Some(e.to_string()),
            ..Advisory::default()
        },
    }
}

/// Record one firing per `(unit, match)` pair, keyed by the match. The site
/// is the unit's declaration line (added) or first added line (modified), so
/// deleting or rewriting the copy reads `changed`.
fn record_firings(
    db: &Db,
    workspace_root: &Path,
    report: &DupReport,
    (surface, diff_spec): (&str, &str),
    scope: &DiffScope,
    patch: &ReviewedPatch,
) -> Result<usize> {
    let anchor = git::head_commit_hash(workspace_root);
    let ctx = FiringContext {
        surface,
        diff_spec,
        base_rev: Some(&scope.base_revision),
        head_rev: scope.head_revision.as_deref(),
        anchor_commit: anchor.as_deref(),
    };
    let event_id = crate::tools::firings::resolve_event(db, workspace_root, &ctx, patch)?;
    let snippets = advisory::line_snippets(
        workspace_root,
        scope,
        report
            .findings
            .iter()
            .map(|f| (report.unit(f).hrr.file_path.as_str(), f.line)),
    )?;
    let keys: Vec<Vec<String>> = report
        .findings
        .iter()
        .map(|f| {
            f.matches
                .iter()
                .map(|m| {
                    let matched = report.matched(m);
                    format!("{}:{}", matched.hrr.file_path, matched.qualified_name)
                })
                .collect()
        })
        .collect();
    let records: Vec<FiringRecord<'_>> = report
        .findings
        .iter()
        .zip(&snippets)
        .zip(&keys)
        .flat_map(|((f, snippet), keys)| {
            let unit = report.unit(f);
            keys.iter().map(move |key| FiringRecord {
                mechanism: MECHANISM,
                finding_kind: f.kind.as_str(),
                finding_key: key,
                file_path: &unit.hrr.file_path,
                line: Some(f.line),
                symbol: Some(&unit.qualified_name),
                snippet: Some(snippet),
                occurrence: 0,
            })
        })
        .collect();
    db.record_firings(event_id, &records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_only_drops_the_signature() {
        assert_eq!(body_only("fn f(a: u32) -> u32 {\n    a\n}"), "{\n    a\n}");
        assert_eq!(body_only("int f(int a) => a + 1;"), "=> a + 1;");
        assert_eq!(
            body_only("def f(a):\n    return a"),
            "def f(a):\n    return a"
        );
    }

    /// A Python function renamed, re-parameterized, decorated and moved into
    /// a class scores the same body: the tree, not the text, finds where the
    /// signature ends (sutra/506).
    #[test]
    fn a_python_body_is_read_from_the_tree_without_its_signature() {
        let source = "\
def load_rows(conn):
    rows = conn.execute(\"SELECT id FROM rows\")
    kept = {r.id: r for r in rows if r.score > 10}
    return kept


class Store:
    @cached(ttl=60)
    async def fetch_all(
        self, db: Db, *, cutoff: int = 3
    ) -> dict:
        rows = conn.execute(\"SELECT id FROM rows\")
        kept = {r.id: r for r in rows if r.score > 10}
        return kept
";
        let registry = crate::parser::adapter::default_registry();
        let adapter = registry
            .adapter_for_language("python")
            .expect("invariant: python is registered");
        let tree = ParserPool::new(std::time::Duration::from_secs(5))
            .tree(adapter, source)
            .expect("invariant: the fixture parses");
        let spans = body_spans(&tree);
        let body = |start: usize, end: usize| {
            let text = span_text(source, start, end);
            let at = tree_body_start(&spans, &text, start, end);
            text[at..].to_string()
        };
        let (plain, method) = (body(1, 4), body(8, 14));
        assert!(plain.starts_with("rows = "), "{plain:?}");
        assert!(method.starts_with("rows = "), "{method:?}");
        let terms = |b: &str| subtokens(b).collect::<Vec<_>>();
        assert_eq!(terms(&plain), terms(&method));
        assert_eq!(Shingled::new(&plain).set(), Shingled::new(&method).set());
    }

    /// A docstring is prose, not code: it is cut like a Rust doc comment, so
    /// two functions sharing only a docstring share no lexical evidence
    /// (sutra/509).
    #[test]
    fn a_python_docstring_is_not_part_of_the_body() {
        let source = "\
def load_rows(conn):
    \"\"\"Load every row whose score clears the cutoff.\"\"\"
    rows = conn.execute(\"SELECT id FROM rows\")
    return rows


def plain(conn):
    rows = conn.execute(\"SELECT id FROM rows\")
    return rows


def only_doc():
    \"\"\"Nothing here yet.\"\"\"


def lone_string(x):
    return x
";
        let registry = crate::parser::adapter::default_registry();
        let adapter = registry
            .adapter_for_language("python")
            .expect("invariant: python is registered");
        let tree = ParserPool::new(std::time::Duration::from_secs(5))
            .tree(adapter, source)
            .expect("invariant: the fixture parses");
        let spans = body_spans(&tree);
        let body = |start: usize, end: usize| {
            let text = span_text(source, start, end);
            let at = tree_body_start(&spans, &text, start, end);
            text[at..].to_string()
        };
        let (documented, plain) = (body(1, 4), body(7, 9));
        assert!(documented.starts_with("rows = "), "{documented:?}");
        assert_eq!(documented, plain);
        assert_eq!(body(12, 13).trim(), "", "a docstring-only body is empty");
        assert!(body(16, 17).starts_with("return x"));
    }

    #[test]
    fn subtokens_split_identifiers_and_drop_keywords() {
        let got: Vec<String> = subtokens("let mut rowCount = load_rows(self);").collect();
        assert_eq!(got, vec!["row", "count", "load", "rows"]);
    }

    #[test]
    fn longest_shared_run_is_quoted_from_the_source() {
        let a = "{ let x = conn.prepare(\"SELECT id FROM t\")?.query_map([], row)?; x }";
        let b = "{ let y = conn.prepare(\"SELECT id FROM t\")?.query_map([], row)?; y }";
        let shared = Shingled::new(a)
            .longest_shared(a, &Shingled::new(b).set())
            .expect("a 12-token run is shared");
        assert_eq!(
            shared,
            "= conn.prepare(\"SELECT id FROM t\")?.query_map([], row)?;"
        );
    }

    #[test]
    fn a_shingle_held_by_the_unit_itself_still_counts_as_rare() {
        let mut lex = Lexicon::default();
        // Shingle 7 is in docs 0..4; doc 0 is the unit's own document.
        for d in 0..4 {
            lex.add_shingles(d, &[7]);
        }
        assert_eq!(lex.block_scores(&[7], &[7], 4), vec![1, 1, 1, 1]);
        lex.add_shingles(4, &[7]);
        assert_eq!(lex.block_scores(&[7], &[7], 5), vec![0; 5]);
    }

    #[test]
    fn span_text_is_one_based_and_inclusive() {
        assert_eq!(span_text("a\nb\nc\nd", 2, 3), "b\nc");
    }

    /// Past the encode cap, the embed vectors a strip-only workspace lacks go
    /// to the query's closest candidates, not the first in index order: 30
    /// unrelated functions indexed ahead of the original must not take its
    /// embed score (sutra/510).
    #[test]
    fn missing_embed_vectors_are_encoded_closest_first() {
        let ws_dir = tempfile::tempdir().expect("tempdir");
        let db_dir = tempfile::tempdir().expect("tempdir");
        let filler: String = (0..30)
            .map(|i| {
                format!(
                    "pub fn filler_{i}(x: u32) -> u32 {{\n    let a = x + {i};\n    \
                     let b = a * 3;\n    let c = b ^ a;\n    c\n}}\n"
                )
            })
            .collect();
        let pair = concat!(
            "pub fn load_waivers(conn: &Conn, rule: &str) -> Vec<Waiver> {\n",
            "    let mut stmt = conn.prepare(WAIVER_SELECT).expect(\"invariant: static sql\");\n",
            "    let rows = stmt.query_map([rule], waiver_from_row).expect(\"invariant: bound\");\n",
            "    let waivers: Vec<Waiver> = rows.filter_map(|w| w.ok()).collect();\n",
            "    waivers.into_iter().filter(|w| !w.expired()).collect()\n",
            "}\n",
            "pub fn active_waivers(conn: &Conn, rule_id: &str) -> Vec<Waiver> {\n",
            "    let mut stmt = conn.prepare(WAIVER_SELECT).expect(\"invariant: static sql\");\n",
            "    let rows = stmt.query_map([rule_id], waiver_from_row).expect(\"invariant: bound\");\n",
            "    let found: Vec<Waiver> = rows.filter_map(|w| w.ok()).collect();\n",
            "    found.into_iter().filter(|w| !w.expired()).collect()\n",
            "}\n",
        );
        std::fs::create_dir_all(ws_dir.path().join("src")).expect("mkdir");
        std::fs::write(ws_dir.path().join("src/a_filler.rs"), filler).expect("write");
        std::fs::write(ws_dir.path().join("src/z_pair.rs"), pair).expect("write");
        let ws = crate::workspace::WorkspaceEntry {
            id: "encode-order".to_string(),
            root: ws_dir.path().to_path_buf(),
            languages: vec!["rust".to_string()],
            frozen: false,
        };
        let config = crate::config::Config {
            db_dir: db_dir.path().to_path_buf(),
            workspaces_path: db_dir.path().join("workspaces.toml"),
            listen_addr: "127.0.0.1:0".to_string(),
            parse_parallelism: 1,
            log_level: "warn".to_string(),
            constraints_idle_timeout_sec: 1800,
            parse_timeout_ms: 5000,
        };
        let db = Db::open_unchecked(&ws.id, db_dir.path()).expect("db");
        let registry = crate::parser::adapter::default_registry();
        let cancel = std::sync::atomic::AtomicBool::new(false);
        crate::pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).expect("parse");
        assert!(db.delete_embed_vectors().expect("delete") > 30);
        let query = db
            .resolve_symbol("active_waivers", None)
            .expect("resolve")
            .expect("indexed");

        let n = rank_neighbours(&db, &ws.root, &registry, query.id, 3, 0.0, 5)
            .expect("rank")
            .expect("a function");
        let top = &n.matches[0];
        assert_eq!(n.matched(top).qualified_name, "load_waivers");
        assert!(top.embed > 0.5, "the original was encoded: {}", top.embed);
        assert!(
            n.incomplete.iter().any(|i| i.contains("closest")),
            "{:?}",
            n.incomplete
        );
    }
}
