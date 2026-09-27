//! "You fixed 1 of N": the sibling-pattern advisory (sutra/467).
//!
//! A diff that rewrites an idiom at one site often leaves the same idiom alive
//! at sibling sites. This check extracts the small named idioms a diff removed,
//! keeps those it rewrote or wrapped (not moved, not reformatted), and lists
//! where each still survives in the tree. It is advisory and never gates.
//!
//! Design, noise controls and the back-test that justified shipping it:
//! `docs/sibling-pattern-backtest.md`. This is a port of the frozen v6
//! prototype (`experiments/sibling-pattern/proto.py`) with the regex tokenizer
//! replaced by tree-sitter leaves, move/reformat detection by
//! [`classify_symbols`] and [`resolve_renames`], and the wrap signal by
//! `callee_diff` (sutra/494). Survivors are searched in the diff's head-side
//! snapshot, never the worktree unless that is the reviewed side; there the
//! index (call refs, `string_literals`) picks the files to read. The scan is
//! capped by a [`Budget`]. `tests/sibling-backtest-test.rs` asserts the
//! back-test and noise ceiling; `experiments/sibling-pattern/run.sh` prints
//! them.
//!
//! Idioms, extracted from the removed lines of each source hunk:
//!
//! - `chain`: `a(…).b(…)`, where at least one of a and b is not a generic std
//!   method.
//! - `argfld`: `f(… x.field …)` with f non-generic. Reported only when some
//!   `f(…).g` chain was rewritten in the same diff.
//! - `litset`: string literals in value position close to each other, one
//!   item per removed hunk.
//! - `sql`: the first 40 chars of a string literal of 30 or more chars.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use serde::Serialize;
use tree_sitter::{Node, Parser, Tree};

use crate::db::Db;
use crate::db::firings::{FiringContext, FiringRecord};
use crate::error::{Result, SutraError};
use crate::git;
use crate::parser::adapter::{
    LanguageAdapter, LanguageRegistry, ParseContext, ParserPool, line_in_ranges, node_text,
    path_has_dir_segment,
};
use crate::parser::literals;
use crate::parser::{ExtractedSymbol, ParseResult, SymbolSpan, flatten_symbols, symbol_spans};
use crate::tools::firings::{PatchHasher, ReviewedPatch, count_snippet};
use crate::tools::review::DiffScope;
use crate::tools::symbol_diff::{
    ChangeKind, UnmatchedSymbol, build_unmatched, classify_symbols, resolve_renames,
};

/// The mechanism name in the firing log.
pub const MECHANISM: &str = "sibling_pattern";

/// A finding whose idiom survives at more sites than this is ubiquitous, not
/// a missed sibling.
const MAX_SURVIVORS: usize = 25;
/// Pre-change repo-wide occurrence ceiling for an idiom.
const MAX_DF: usize = 40;
/// Two literals further apart than this many tokens are not one list.
const PAIR_TOKENS: usize = 30;
const MIN_SQL: usize = 30;
/// The index keys a literal by this prefix, so an SQL idiom's prefix is its key.
const SQL_PREFIX: usize = literals::PREFIX_CHARS;
/// Longest literal (quotes included) that can be a list item.
const MAX_LIT: usize = 40;
const _: () = assert!(
    MAX_LIT <= literals::PREFIX_CHARS,
    "a list item must be its own index key"
);
/// Lines around a hunk in which a re-added idiom counts as wrapped in place.
const NEAR: usize = 40;
/// An idiom removed at this many hunks is a sweep; its survivors are the
/// canonical remainder, not misses.
const SWEEP_HUNKS: usize = 3;

const KEYWORDS: &[&str] = &[
    "fn",
    "let",
    "mut",
    "pub",
    "if",
    "else",
    "match",
    "for",
    "in",
    "while",
    "loop",
    "return",
    "self",
    "Self",
    "Some",
    "None",
    "Ok",
    "Err",
    "true",
    "false",
    "as",
    "ref",
    "impl",
    "struct",
    "enum",
    "use",
    "mod",
    "crate",
    "super",
    "where",
    "const",
    "static",
    "async",
    "await",
    "move",
    "dyn",
    "break",
    "continue",
    "trait",
    "type",
    "unsafe",
    "extern",
    "final",
    "var",
    "void",
    "new",
    "this",
    "null",
    "class",
    "extends",
    "implements",
    "with",
    "late",
    "required",
    "import",
    "export",
    "library",
    "part",
    "of",
    "is",
    "try",
    "catch",
    "on",
    "throw",
    "rethrow",
    "switch",
    "case",
    "default",
    "do",
    "get",
    "set",
    "factory",
    "override",
];

/// Standard-library and ubiquitous method names (Rust and Dart). A chain of
/// two of these, or an argfld on one, is an idiom of the language, not of the
/// codebase. The stoplist is the weakest part of the design; the DF ceiling
/// catches a codebase's own ubiquitous helpers.
const GENERIC: &[&str] = &[
    "iter",
    "iter_mut",
    "into_iter",
    "map",
    "filter",
    "filter_map",
    "flat_map",
    "flatten",
    "collect",
    "entry",
    "or_insert",
    "or_insert_with",
    "or_default",
    "push",
    "push_str",
    "insert",
    "get",
    "get_mut",
    "contains",
    "contains_key",
    "remove",
    "unwrap",
    "expect",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "and_then",
    "or_else",
    "ok",
    "err",
    "ok_or",
    "ok_or_else",
    "map_err",
    "map_or",
    "map_or_else",
    "as_ref",
    "as_deref",
    "as_mut",
    "as_str",
    "as_bytes",
    "as_slice",
    "to_string",
    "to_owned",
    "to_vec",
    "clone",
    "cloned",
    "copied",
    "into",
    "find",
    "find_map",
    "any",
    "all",
    "position",
    "enumerate",
    "take",
    "skip",
    "zip",
    "chain",
    "rev",
    "sum",
    "count",
    "len",
    "is_empty",
    "keys",
    "values",
    "sort",
    "sort_by",
    "sort_by_key",
    "sort_unstable",
    "dedup",
    "split",
    "lines",
    "trim",
    "strip_prefix",
    "strip_suffix",
    "starts_with",
    "ends_with",
    "join",
    "extend",
    "windows",
    "chunks",
    "max",
    "min",
    "max_by",
    "min_by",
    "fold",
    "last",
    "first",
    "next",
    "peekable",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "transpose",
    "lock",
    "read",
    "write",
    "send",
    "json",
    "post",
    "parse",
    "format",
    "query_map",
    "prepare",
    "prepare_cached",
    "execute",
    "query_row",
    "display",
    "to_lowercase",
    "to_uppercase",
    "partial_cmp",
    "cmp",
    "eq",
    "retain",
    "drain",
    "split_whitespace",
    "chars",
    "bytes",
    "to_hex",
    "from",
    "new",
    "default",
    "with_capacity",
    "then",
    "then_some",
    "toList",
    "where",
    "firstWhere",
    "add",
    "addAll",
    "containsKey",
    "putIfAbsent",
    "extension",
    "file_name",
    "file_stem",
    "parent",
    "exists",
    "is_file",
    "is_dir",
    "to_str",
    "to_string_lossy",
    "canonicalize",
    "elapsed",
    "as_millis",
    "as_secs",
    "as_secs_f64",
    "now",
    "duration_since",
    "saturating_sub",
    "saturating_add",
    "checked_sub",
    "checked_add",
    "abs",
    "round",
    "floor",
    "ceil",
    "powi",
    "sqrt",
    "pop",
    "push_back",
    "is_some_and",
    "is_none_or",
    "then_with",
];

/// Path segments that hold tests, benches or examples, in any language.
const EXCLUDED_DIRS: &[&str] = &[
    "test",
    "tests",
    "benches",
    "test_driver",
    "integration_test",
    "examples",
];

static KEYWORD_SET: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| KEYWORDS.iter().copied().collect());
static GENERIC_SET: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| GENERIC.iter().copied().collect());
static TEST_FILE: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"[-_]test\.(rs|dart)$").expect("invariant: static regex compiles")
});

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdiomKind {
    Chain,
    Argfld,
    Litset,
    Sql,
}

impl IdiomKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chain => "chain",
            Self::Argfld => "argfld",
            Self::Litset => "litset",
            Self::Sql => "sql",
        }
    }
}

/// How the diff treated the idiom where it was removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternClass {
    /// The diff lowered the idiom's count in the files it touched.
    Rewritten,
    /// Count unchanged, but re-added in place next to a new call: the old code
    /// kept inside a new branch.
    Wrapped,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Idiom {
    pub kind: IdiomKind,
    pub idiom: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Survivor {
    pub file: String,
    pub line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// The survivor's line, trimmed. Recorded in the firing log.
    #[serde(skip)]
    pub snippet: String,
    /// The survivor's ordinal among identical lines in its symbol (in the file
    /// when there is none). Part of its site identity in the firing log.
    #[serde(skip)]
    pub occurrence: usize,
}

/// One idiom (or several with the same survivors) that the diff removed and
/// that survives elsewhere.
#[derive(Debug, Clone, Serialize)]
pub struct SiblingFinding {
    pub idioms: Vec<Idiom>,
    pub class: PatternClass,
    pub removed_at: Vec<String>,
    pub survivors: Vec<Survivor>,
    pub survivor_count: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct SiblingReport {
    pub findings: Vec<SiblingFinding>,
    /// Files the check could not read or parse. Non-empty means the result is
    /// incomplete, never clean.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub incomplete: Vec<String>,
    /// Identity of the reviewed change, for the firing log.
    #[serde(skip)]
    pub patch: ReviewedPatch,
}

/// The three noise controls added after the back-test (docs "Next noise
/// controls"). All on by default. `SUTRA_SIBLING_CONTROLS_OFF` (a comma list of
/// `canonical_mapping`, `group_by_survivors`, `grown_litset`) turns them off so
/// the harness can measure each one.
#[derive(Debug, Clone, Copy)]
pub struct Controls {
    /// Drop literal survivors that sit in a `Path::Variant => "lit"` arm.
    pub canonical_mapping: bool,
    /// Merge findings that point at the same survivor set.
    pub group_by_survivors: bool,
    /// A literal list that grew (added side a strict superset of the removed
    /// side) is a list extension: treat it as rewritten.
    pub grown_litset: bool,
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            canonical_mapping: true,
            group_by_survivors: true,
            grown_litset: true,
        }
    }
}

impl Controls {
    pub fn from_env() -> Self {
        let off = std::env::var_os("SUTRA_SIBLING_CONTROLS_OFF");
        let off = off
            .as_deref()
            .map(|v| v.to_string_lossy())
            .unwrap_or_default();
        let off: HashSet<&str> = off.split(',').map(str::trim).collect();
        Self {
            canonical_mapping: !off.contains("canonical_mapping"),
            group_by_survivors: !off.contains("group_by_survivors"),
            grown_litset: !off.contains("grown_litset"),
        }
    }
}

/// How long the survivor scan over unchanged files may run. Review latency
/// otherwise grows with the tree, most of all for a snapshot side, where the
/// index cannot narrow and every candidate file is read. A scan that runs out
/// stops and says so in `incomplete`; it never reads as clean.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub scan_time: std::time::Duration,
}

impl Budget {
    const DEFAULT_MS: u64 = 5_000;

    /// The default, or `SUTRA_SIBLING_BUDGET_MS` when set to a number.
    pub fn from_env() -> Self {
        let ms = std::env::var("SUTRA_SIBLING_BUDGET_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(Self::DEFAULT_MS);
        Self {
            scan_time: std::time::Duration::from_millis(ms),
        }
    }
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            scan_time: std::time::Duration::from_millis(Self::DEFAULT_MS),
        }
    }
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokKind {
    Ident,
    Str,
    RawStr,
    Char,
    Other,
}

#[derive(Debug)]
struct Token {
    text: String,
    line: usize,
    kind: TokKind,
}

fn is_comment(kind: &str) -> bool {
    kind.contains("comment")
}

/// The leaf tokens of `tree`, comments dropped, with string literals kept
/// whole. A Rust lifetime is one opaque token, so `'a` never reads as a name.
fn tokenize(tree: &Tree, src: &[u8]) -> Vec<Token> {
    let mut out = Vec::new();
    let mut stack: Vec<Node> = vec![tree.root_node()];
    let mut cursor = tree.walk();
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if is_comment(kind) {
            continue;
        }
        let line = node.start_position().row + 1;
        let whole = match kind {
            "string_literal" => Some(string_kind(node_text(node, src))),
            "raw_string_literal" => Some(TokKind::RawStr),
            "char_literal" => Some(TokKind::Char),
            "lifetime" => Some(TokKind::Other),
            _ => None,
        };
        if let Some(tok_kind) = whole {
            let text = node_text(node, src);
            let text = if tok_kind == TokKind::Other {
                text.to_string()
            } else {
                literals::normalize(text)
            };
            out.push(Token {
                text,
                line,
                kind: tok_kind,
            });
            continue;
        }
        if node.child_count() == 0 {
            let text = node_text(node, src);
            if text.is_empty() {
                continue;
            }
            out.push(Token {
                text: text.to_string(),
                line,
                kind: if is_ident(text) {
                    TokKind::Ident
                } else {
                    TokKind::Other
                },
            });
            continue;
        }
        let base = stack.len();
        cursor.reset(node);
        if cursor.goto_first_child() {
            loop {
                stack.push(cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        stack[base..].reverse();
    }
    out
}

/// A Dart single-character literal (`'a'`) reads as a char, like Rust's.
fn string_kind(text: &str) -> TokKind {
    let mut chars = text.chars();
    match (chars.next(), chars.next(), chars.next(), chars.next()) {
        (Some('\''), Some(_), Some('\''), None) => TokKind::Char,
        _ => TokKind::Str,
    }
}

fn is_ident(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// Idiom extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Feature {
    /// `head.method`: head is `name`, `a::name` or `.name`.
    Chain {
        head: String,
        method: String,
    },
    Argfld {
        callee: String,
        field: String,
    },
    /// Two literals, sorted.
    Litpair(String, String),
    Sql(String),
    /// A literal list the diff extended (`grown_litset`): the old items and
    /// the added ones, sorted. It survives wherever a list still holds every
    /// old item and none of the added ones.
    GrownList {
        old: Vec<String>,
        added: Vec<String>,
    },
}

impl Feature {
    fn text(&self) -> String {
        match self {
            Self::Chain { head, method } => format!("{head}.{method}"),
            Self::Argfld { callee, field } => format!("{callee}(.{field})"),
            Self::Litpair(a, b) => format!("{a}+{b}"),
            Self::Sql(prefix) => prefix.to_string(),
            Self::GrownList { old, added } => {
                format!("{{{}}} extended by {}", old.join(", "), added.join(", "))
            }
        }
    }

    /// The bare name of a call every occurrence contains, so the index can
    /// narrow the survivor search.
    fn call_name(&self) -> Option<&str> {
        match self {
            Self::Chain { method, .. } => Some(method),
            Self::Argfld { callee, .. } => Some(bare_name(callee)),
            Self::Litpair(..) | Self::Sql(_) | Self::GrownList { .. } => None,
        }
    }

    /// Index keys of the literals every occurrence contains, so the index can
    /// narrow the survivor search for literal and SQL idioms.
    fn literal_keys(&self) -> Vec<String> {
        match self {
            Self::Litpair(a, b) => vec![literals::index_key(a), literals::index_key(b)],
            Self::GrownList { old, .. } => old.iter().map(|o| literals::index_key(o)).collect(),
            Self::Sql(prefix) => vec![literals::index_key(prefix)],
            Self::Chain { .. } | Self::Argfld { .. } => Vec::new(),
        }
    }

    /// Substrings every file holding an occurrence must contain.
    fn needles(&self) -> Vec<&str> {
        match self {
            Self::Chain { method, .. } => vec![method],
            Self::Argfld { callee, field } => vec![bare_name(callee), field],
            Self::Litpair(a, b) => vec![a, b],
            Self::GrownList { old, .. } => old.iter().map(String::as_str).collect(),
            Self::Sql(prefix) => prefix
                .split(' ')
                .max_by_key(|s| s.len())
                .into_iter()
                .collect(),
        }
    }
}

fn bare_name(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name)
}

/// Whether extraction reads a removed hunk or a whole tree (survivors, counts).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scope {
    Removed,
    /// `canonical_mapping`: skip literals that sit in a mapping arm.
    Tree {
        canonical_mapping: bool,
    },
}

fn skip_group(toks: &[Token], mut i: usize, open: &str, close: &str) -> usize {
    let mut depth = 0i32;
    while i < toks.len() {
        if toks[i].text == open {
            depth += 1;
        } else if toks[i].text == close {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
        i += 1;
    }
    i
}

struct Call {
    name: String,
    paren: usize,
}

impl Call {
    fn bare(&self) -> &str {
        bare_name(&self.name)
    }
}

/// If `toks[i]` is a called identifier: its name (`f`, `a::f` or `.f`) and the
/// index of its `(`. Macros are not calls.
fn callee_at(toks: &[Token], i: usize) -> Option<Call> {
    let tok = &toks[i];
    if tok.kind != TokKind::Ident || KEYWORD_SET.contains(tok.text.as_str()) {
        return None;
    }
    let mut j = i + 1;
    if toks.get(j).is_some_and(|t| t.text == "!") {
        return None;
    }
    if toks.get(j).is_some_and(|t| t.text == "::") && toks.get(j + 1).is_some_and(|t| t.text == "<")
    {
        j = skip_group(toks, j + 1, "<", ">");
    }
    if toks.get(j).is_none_or(|t| t.text != "(") {
        return None;
    }
    let name = if i >= 2 && toks[i - 1].text == "::" && toks[i - 2].kind == TokKind::Ident {
        format!("{}::{}", toks[i - 2].text, tok.text)
    } else if i >= 1 && matches!(toks[i - 1].text.as_str(), "." | "?.") {
        format!(".{}", tok.text)
    } else {
        tok.text.to_string()
    };
    Some(Call { name, paren: j })
}

fn is_generic(bare: &str, name: &str) -> bool {
    !name.contains("::") && GENERIC_SET.contains(bare)
}

/// A string literal in value position: not a map key, not a format template,
/// not bare punctuation.
fn value_literal(toks: &[Token], i: usize) -> bool {
    let tok = &toks[i].text;
    let len = tok.chars().count();
    if !(2 < len && len <= MAX_LIT) {
        return false;
    }
    if tok.contains('{') || !tok.chars().any(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let next = toks.get(i + 1).map(|t| t.text.as_str());
    if next == Some(":") {
        return false;
    }
    if next == Some(".") && toks.get(i + 2).is_some_and(|t| t.text == "into") {
        return false; // m.insert("key".into(), …)
    }
    if i >= 2
        && toks[i - 1].text == "("
        && matches!(toks[i - 2].text.as_str(), "insert" | "get" | "contains_key")
    {
        return false;
    }
    true
}

/// `Path::Variant` (or Dart `Enum.variant`) ending just before `end`.
fn path_ends_at(toks: &[Token], end: usize) -> bool {
    end >= 3
        && toks[end - 1].kind == TokKind::Ident
        && matches!(toks[end - 2].text.as_str(), "::" | ".")
        && toks[end - 3].kind == TokKind::Ident
}

/// `Path::Variant` (or Dart `Enum.variant`) starting at `start`.
fn path_starts_at(toks: &[Token], start: usize) -> bool {
    toks.get(start).is_some_and(|t| t.kind == TokKind::Ident)
        && toks
            .get(start + 1)
            .is_some_and(|t| matches!(t.text.as_str(), "::" | "."))
        && toks
            .get(start + 2)
            .is_some_and(|t| t.kind == TokKind::Ident)
}

/// A literal in a canonical enum mapping arm: `Path::Variant => "lit"` or
/// `"lit" => Path::Variant`. These are where a stringly list's values are
/// supposed to live after an enum sweep, not missed sites.
fn mapping_literal(toks: &[Token], i: usize) -> bool {
    (i >= 1 && toks[i - 1].text == "=>" && path_ends_at(toks, i - 1))
        || (toks.get(i + 1).is_some_and(|t| t.text == "=>") && path_starts_at(toks, i + 2))
}

/// Idioms in `toks` and the lines they occur on.
fn features(toks: &[Token], scope: Scope) -> BTreeMap<Feature, Vec<usize>> {
    let mut out: BTreeMap<Feature, Vec<usize>> = BTreeMap::new();
    for i in 0..toks.len() {
        let tok = &toks[i];
        if let Some(call) = callee_at(toks, i) {
            let end = skip_group(toks, call.paren, "(", ")");
            if !is_generic(call.bare(), &call.name) {
                let mut depth = 0i32;
                for k in call.paren..end {
                    match toks[k].text.as_str() {
                        "(" => depth += 1,
                        ")" => depth -= 1,
                        "." if depth == 1
                            && k + 1 < end
                            && toks[k + 1].kind == TokKind::Ident
                            && (k + 2 >= end
                                || !matches!(toks[k + 2].text.as_str(), "(" | "::")) =>
                        {
                            out.entry(Feature::Argfld {
                                callee: call.name.to_string(),
                                field: toks[k + 1].text.to_string(),
                            })
                            .or_default()
                            .push(tok.line);
                        }
                        _ => {}
                    }
                }
            }
            let mut j = end;
            if toks.get(j).is_some_and(|t| t.text == "?") {
                j += 1;
            }
            if toks
                .get(j)
                .is_some_and(|t| matches!(t.text.as_str(), "." | "?."))
                && j + 1 < toks.len()
                && let Some(next) = callee_at(toks, j + 1)
                && !(is_generic(call.bare(), &call.name) && is_generic(next.bare(), &next.name))
            {
                out.entry(Feature::Chain {
                    method: next.bare().to_string(),
                    head: call.name,
                })
                .or_default()
                .push(tok.line);
            }
        } else if matches!(tok.kind, TokKind::Str | TokKind::RawStr)
            && tok.text.chars().count() >= MIN_SQL
        {
            let prefix: String = tok.text.chars().take(SQL_PREFIX).collect();
            out.entry(Feature::Sql(prefix)).or_default().push(tok.line);
        }
    }

    let skip_mapping = matches!(
        scope,
        Scope::Tree {
            canonical_mapping: true
        }
    );
    let lits: Vec<usize> = (0..toks.len())
        .filter(|&i| toks[i].kind == TokKind::Str && value_literal(toks, i))
        .filter(|&i| !(skip_mapping && mapping_literal(toks, i)))
        .collect();
    for (n, &ia) in lits.iter().enumerate() {
        for &ib in &lits[n + 1..] {
            if ib - ia > PAIR_TOKENS {
                break;
            }
            let (a, b) = (&toks[ia].text, &toks[ib].text);
            if a != b {
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                out.entry(Feature::Litpair(lo.to_string(), hi.to_string()))
                    .or_default()
                    .push(toks[ia].line.min(toks[ib].line));
            }
        }
    }
    out
}

/// Runs of value-position literals no more than [`PAIR_TOKENS`] apart: one
/// run is one literal list. Each run's texts and its first line.
fn literal_lists(toks: &[Token], scope: Scope) -> Vec<(BTreeSet<&str>, usize)> {
    let skip_mapping = matches!(
        scope,
        Scope::Tree {
            canonical_mapping: true
        }
    );
    let mut out: Vec<(BTreeSet<&str>, usize)> = Vec::new();
    let mut last: Option<usize> = None;
    for i in 0..toks.len() {
        if toks[i].kind != TokKind::Str
            || !value_literal(toks, i)
            || (skip_mapping && mapping_literal(toks, i))
        {
            continue;
        }
        match (last, out.last_mut()) {
            (Some(prev), Some((set, _))) if i - prev <= PAIR_TOKENS => {
                set.insert(&toks[i].text);
            }
            _ => out.push(([toks[i].text.as_str()].into_iter().collect(), toks[i].line)),
        }
        last = Some(i);
    }
    out
}

/// Lines where each removed idiom of `lang` occurs in a whole file's tokens.
fn tree_occurrences<'k, V>(
    toks: &[Token],
    scope: Scope,
    removed: &'k BTreeMap<Key, V>,
    lang: &'static str,
) -> Vec<(&'k Key, Vec<usize>)> {
    let mut out = Vec::new();
    for (feat, lines) in features(toks, scope) {
        if let Some((key, _)) = removed.get_key_value(&(lang, feat)) {
            out.push((key, lines));
        }
    }
    let mut lists: Option<Vec<(BTreeSet<&str>, usize)>> = None;
    for key in removed.keys() {
        let (l, Feature::GrownList { old, added }) = key else {
            continue;
        };
        if *l != lang {
            continue;
        }
        let lists = lists.get_or_insert_with(|| literal_lists(toks, scope));
        let lines: Vec<usize> = lists
            .iter()
            .filter(|(set, _)| {
                old.iter().all(|o| set.contains(o.as_str()))
                    && !added.iter().any(|a| set.contains(a.as_str()))
            })
            .map(|&(_, line)| line)
            .collect();
        if !lines.is_empty() {
            out.push((key, lines));
        }
    }
    out
}

/// Value-position literals in `toks`, for the grown-list control.
fn literal_set(toks: &[Token]) -> BTreeSet<&str> {
    (0..toks.len())
        .filter(|&i| toks[i].kind == TokKind::Str && value_literal(toks, i))
        .map(|i| toks[i].text.as_str())
        .collect()
}

/// Token multiset without commas: rustfmt adds trailing commas and reorders
/// imports, and neither is a rewrite.
fn token_multiset(toks: &[Token]) -> Vec<&str> {
    let mut v: Vec<&str> = toks
        .iter()
        .map(|t| t.text.as_str())
        .filter(|t| *t != ",")
        .collect();
    v.sort_unstable();
    v
}

/// The tokens on lines `range` (tokens are in line order).
fn tokens_in(toks: &[Token], range: std::ops::Range<usize>) -> &[Token] {
    let lo = toks.partition_point(|t| t.line < range.start);
    let hi = toks.partition_point(|t| t.line < range.end);
    &toks[lo..hi]
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// The two languages the check was back-tested on. Its stoplist is Rust and
/// Dart only.
fn supported(adapter: &dyn LanguageAdapter) -> bool {
    matches!(adapter.language_id(), "rust" | "dart")
}

/// Test, bench and example code is never a survivor or a removal site.
fn excluded_path(adapter: &dyn LanguageAdapter, path: &str) -> bool {
    adapter.is_test_path(path)
        || EXCLUDED_DIRS.iter().any(|d| path_has_dir_segment(path, d))
        || TEST_FILE.is_match(path)
}

fn adapter_for_path<'r>(
    registry: &'r LanguageRegistry,
    path: &str,
) -> Option<&'r dyn LanguageAdapter> {
    let ext = Path::new(path).extension()?.to_str()?;
    registry
        .adapter_for_extension(ext)
        .filter(|a| supported(*a))
}

/// A file's tokens outside its test-only items.
struct FileTokens {
    tokens: Vec<Token>,
    lines: Vec<String>,
}

struct Parsers {
    ts: HashMap<&'static str, Parser>,
    pool: ParserPool,
}

impl Parsers {
    fn new() -> Self {
        Self {
            ts: HashMap::new(),
            pool: ParserPool::new(std::time::Duration::from_secs(5)),
        }
    }

    fn tokens(
        &mut self,
        adapter: &dyn LanguageAdapter,
        source: &str,
        path: &str,
    ) -> Result<FileTokens> {
        let lang = language_key(adapter);
        let parser = match self.ts.entry(lang) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let mut p = Parser::new();
                p.set_language(&adapter.grammar())
                    .map_err(|err| SutraError::Parse(format!("set language {lang}: {err}")))?;
                e.insert(p)
            }
        };
        let tree = parser
            .parse(source, None)
            .ok_or_else(|| SutraError::Parse(format!("{path}: tree-sitter returned no tree")))?;
        let ctx = ParseContext {
            source: source.as_bytes(),
            tree: &tree,
            file_path: path,
        };
        let test_ranges = adapter.test_line_ranges(&ctx);
        let mut tokens = tokenize(&tree, source.as_bytes());
        tokens.retain(|t| u32::try_from(t.line).is_ok_and(|l| !line_in_ranges(&test_ranges, l)));
        Ok(FileTokens {
            tokens,
            lines: source.lines().map(str::to_string).collect(),
        })
    }

    fn parse(
        &mut self,
        adapter: &dyn LanguageAdapter,
        source: &str,
        path: &str,
    ) -> Result<ParseResult> {
        self.pool.parse_with(adapter, source, path)
    }
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

type Key = (&'static str, Feature);
type Site<'a> = (&'a str, usize);

/// A file the diff touched, both sides tokenized.
struct Changed<'a> {
    new_path: Option<&'a str>,
    old: Option<FileTokens>,
    new: Option<FileTokens>,
    added: HashSet<usize>,
    language: &'static str,
}

#[derive(Default)]
struct Removed<'a> {
    sites: BTreeSet<(&'a str, usize)>,
    /// `(new path, new_start)` of each hunk it was removed from.
    origins: Vec<Site<'a>>,
    hunks: BTreeSet<usize>,
}

fn language_key(adapter: &dyn LanguageAdapter) -> &'static str {
    match adapter.language_id() {
        "dart" => "dart",
        _ => "rust",
    }
}

/// What symbol classification says about one changed file (sutra/494): which
/// old code the diff kept, and which new symbols gained a call.
#[derive(Default)]
struct SymbolSignals {
    /// Old-side spans whose substance the diff kept: reformatted
    /// (`CosmeticChanged`), unchanged but shifted within the file, or moved to
    /// another file (added once [`resolve_renames`] has seen every file). A
    /// hunk inside one rewrote nothing.
    kept_old: Vec<(usize, usize)>,
    /// New-side symbol spans, each with whether `callee_diff` says the symbol
    /// gained a non-generic callee.
    new_spans: Vec<(usize, usize, bool)>,
}

impl SymbolSignals {
    /// Classify one file changed on both sides, returning the symbols
    /// classification left unmatched for the cross-file move pass.
    fn classify(
        parses: (&ParseResult, &ParseResult),
        sources: (&str, &str),
        paths: (&str, &str),
    ) -> (Self, Vec<UnmatchedSymbol>, Vec<UnmatchedSymbol>) {
        let (old_parse, new_parse) = parses;
        let result = classify_symbols(old_parse, new_parse, sources.0, sources.1, paths.0, paths.1);
        let changed: HashSet<&str> = result.changes.iter().map(|c| c.symbol.as_str()).collect();
        let cosmetic: HashSet<&str> = result
            .changes
            .iter()
            .filter(|c| c.change == ChangeKind::CosmeticChanged)
            .map(|c| c.symbol.as_str())
            .collect();
        let gained: HashSet<&str> = result
            .changes
            .iter()
            .filter(|c| {
                c.callee_diff
                    .as_ref()
                    .is_some_and(|cd| cd.added.iter().any(|n| !is_generic(n, n)))
            })
            .map(|c| c.symbol.as_str())
            .collect();
        let new_flat = flatten_symbols(&new_parse.symbols);
        let new_keys: HashSet<(&str, &str)> = new_flat
            .iter()
            .map(|s| (s.qualified_name.as_str(), s.kind.as_str()))
            .collect();
        let kept_old = flatten_symbols(&old_parse.symbols)
            .into_iter()
            .filter(|s| {
                let name = s.qualified_name.as_str();
                cosmetic.contains(name)
                    || (new_keys.contains(&(name, s.kind.as_str())) && !changed.contains(name))
            })
            .map(span)
            .collect();
        let new_spans = new_flat
            .iter()
            .map(|s| {
                let (start, end) = span(s);
                (start, end, gained.contains(s.qualified_name.as_str()))
            })
            .collect();
        (
            Self {
                kept_old,
                new_spans,
            },
            result.unmatched_old,
            result.unmatched_new,
        )
    }

    /// Whether the removed lines sit inside code the diff kept.
    fn kept(&self, removed: &std::ops::Range<usize>) -> bool {
        self.kept_old
            .iter()
            .any(|&(s, e)| s <= removed.start && removed.end <= e + 1)
    }

    /// Whether the innermost symbols around a hunk's added lines gained a
    /// non-generic callee: new code wrapped around the old.
    fn gains_callee(&self, added: &std::ops::Range<usize>) -> bool {
        let Some(last) = added.end.checked_sub(1) else {
            return false;
        };
        let around: Vec<&(usize, usize, bool)> = self
            .new_spans
            .iter()
            .filter(|(s, e, _)| *s <= last && added.start <= *e)
            .collect();
        around
            .iter()
            .filter(|a| {
                !around
                    .iter()
                    .any(|b| a.0 <= b.0 && b.1 <= a.1 && (a.0, a.1) != (b.0, b.1))
            })
            .any(|a| a.2)
    }
}

fn span(s: &ExtractedSymbol) -> (usize, usize) {
    (s.start_line, s.end_line)
}

/// Old-side spans, by file, of symbols moved to another file with their body
/// (or its structure) intact: the cross-file half of [`SymbolSignals::kept_old`].
fn moved_spans(
    unmatched_old: &[UnmatchedSymbol],
    unmatched_new: &[UnmatchedSymbol],
) -> HashMap<String, Vec<(usize, usize)>> {
    let by_name: HashMap<(&str, &str), &UnmatchedSymbol> = unmatched_old
        .iter()
        .map(|s| ((s.file.as_str(), s.qualified_name.as_str()), s))
        .collect();
    let mut out: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    for (_, change) in resolve_renames(unmatched_old, unmatched_new).changes {
        if change.change != ChangeKind::Moved {
            continue;
        }
        let Some(file) = change.from_file.as_deref() else {
            continue;
        };
        let name = change.from_symbol.as_deref().unwrap_or(&change.symbol);
        if let Some(old) = by_name.get(&(file, name)) {
            let end = old.start_line + old.content.lines().count().max(1) - 1;
            out.entry(file.to_string())
                .or_default()
                .push((old.start_line, end));
        }
    }
    out
}

/// Both sides of one changed file: `(old, new)` source.
pub(crate) fn read_sides(
    workspace_root: &Path,
    scope: &DiffScope,
    fh: &git::FileHunks,
) -> Result<(Option<String>, Option<String>)> {
    let old = match fh.old_path.as_deref() {
        Some(p) => git::file_content_on_side(workspace_root, Some(&scope.base_revision), p)?,
        None => None,
    };
    let new = match fh.new_path.as_deref() {
        Some(p) => git::file_content_on_side(workspace_root, scope.head_revision.as_deref(), p)?,
        None => None,
    };
    Ok((old, new))
}

/// Per-hunk signals gathered from the removed side.
#[derive(Default)]
struct HunkSignals<'a> {
    removed: BTreeMap<Key, Removed<'a>>,
    /// Hunks whose enclosing symbol gained a non-generic callee.
    wraps: HashSet<usize>,
    next_id: usize,
}

impl<'a> HunkSignals<'a> {
    fn collect(
        &mut self,
        fh: &'a git::FileHunks,
        paths: (&'a str, &'a str),
        tokens: (&FileTokens, &FileTokens),
        symbols: &SymbolSignals,
        lang: &'static str,
        controls: Controls,
    ) {
        let (old_path, new_path) = paths;
        let (old_toks, new_toks) = tokens;
        for hunk in &fh.hunks {
            if hunk.old_len == 0 {
                continue;
            }
            self.next_id += 1;
            let hid = self.next_id;
            if hunk.new_len == 0 {
                continue; // pure deletion: deleted code is not a rewritten idiom
            }
            let rem = tokens_in(&old_toks.tokens, hunk.removed_lines());
            let add = tokens_in(&new_toks.tokens, hunk.added_lines());
            if token_multiset(rem) == token_multiset(add) {
                continue; // formatting only
            }
            if symbols.kept(&hunk.removed_lines()) {
                continue; // inside a symbol the diff only reformatted or moved
            }
            if symbols.gains_callee(&hunk.added_lines()) {
                self.wraps.insert(hid);
            }
            let mut found = features(rem, Scope::Removed);
            if controls.grown_litset {
                let (r, a) = (literal_set(rem), literal_set(add));
                if r.len() >= 2 && r.len() < a.len() && r.is_subset(&a) {
                    let line = rem
                        .iter()
                        .find(|t| r.contains(t.text.as_str()))
                        .map_or(hunk.old_start, |t| t.line);
                    let grown = Feature::GrownList {
                        old: r.iter().map(|s| s.to_string()).collect(),
                        added: a.difference(&r).map(|s| s.to_string()).collect(),
                    };
                    found.insert(grown, vec![line]);
                }
            }
            for (feat, lines) in found {
                let entry = self.removed.entry((lang, feat)).or_default();
                entry.sites.extend(lines.iter().map(|&l| (old_path, l)));
                entry.origins.push((new_path, hunk.new_start));
                entry.hunks.insert(hid);
            }
        }
    }
}

/// Where each removed idiom occurs after the change.
#[derive(Default)]
struct Occurrences<'a> {
    pre_changed: HashMap<&'a Key, usize>,
    post_changed: HashMap<&'a Key, usize>,
    unchanged: HashMap<&'a Key, usize>,
    /// Occurrences on lines the diff added.
    readded: HashMap<&'a Key, Vec<Site<'a>>>,
    survivors: HashMap<&'a Key, BTreeSet<Site<'a>>>,
    snippets: HashMap<Site<'a>, String>,
}

impl<'a> Occurrences<'a> {
    fn count(m: &HashMap<&Key, usize>, k: &Key) -> usize {
        m.get(k).copied().unwrap_or(0)
    }

    fn rewritten(&self, k: &Key) -> bool {
        Self::count(&self.pre_changed, k) > Self::count(&self.post_changed, k)
    }

    /// Repo-wide count before the change: unchanged files are the same on
    /// both sides.
    fn pre_df(&self, k: &Key) -> usize {
        Self::count(&self.unchanged, k) + Self::count(&self.pre_changed, k)
    }

    fn survive(&mut self, key: &'a Key, site: Site<'a>, lines: &[String]) {
        self.survivors.entry(key).or_default().insert(site);
        self.snippets
            .entry(site)
            .or_insert_with(|| line_text(lines, site.1));
    }
}

/// The unchanged files of the reviewed snapshot, where survivors are searched.
enum SurvivorTree {
    /// The worktree (an unstaged review): the index lists the files and
    /// narrows each idiom to the files holding its call name (call idioms) or
    /// all of its literals (literal and SQL idioms).
    Worktree {
        files: Vec<crate::db::FileRow>,
        call_files: HashMap<String, HashSet<i64>>,
        literal_files: HashMap<String, HashSet<i64>>,
    },
    /// The index (a staged review) or a commit: git lists the files and every
    /// one is read from that snapshot. The sutra index describes the worktree,
    /// so it neither lists nor narrows here.
    Snapshot { paths: Vec<String> },
}

impl SurvivorTree {
    /// The file list, plus the reader for a snapshot side (`None` reads the
    /// worktree).
    fn open<'k>(
        db: &Db,
        workspace_root: &Path,
        scope: &DiffScope,
        features: impl Iterator<Item = &'k Feature>,
    ) -> Result<(Self, Option<git::SnapshotReader>)> {
        Ok(match scope.head_revision.as_deref() {
            None => {
                let mut call_names = Vec::new();
                let mut literal_keys = Vec::new();
                for f in features {
                    call_names.extend(f.call_name());
                    literal_keys.extend(f.literal_keys());
                }
                let literal_keys: Vec<&str> = literal_keys.iter().map(String::as_str).collect();
                (
                    Self::Worktree {
                        files: db.all_files()?,
                        call_files: db.files_with_calls_named(&call_names)?,
                        literal_files: db.files_with_literals(&literal_keys)?,
                    },
                    None,
                )
            }
            Some(rev) => (
                Self::Snapshot {
                    paths: git::snapshot_files(workspace_root, rev)?
                        .into_iter()
                        .filter(|p| !crate::pipeline::walker_skips(p))
                        .collect(),
                },
                Some(git::SnapshotReader::open(workspace_root, rev)?),
            ),
        })
    }

    /// Each file with its index id, when the index is the file list.
    fn files(&self) -> Vec<(&str, Option<i64>)> {
        match self {
            Self::Worktree { files, .. } => files.iter().map(|f| (&*f.path, Some(f.id))).collect(),
            Self::Snapshot { paths } => paths.iter().map(|p| (p.as_str(), None)).collect(),
        }
    }

    /// Whether the file can hold `feat` as far as the index knows.
    fn may_hold(&self, file_id: Option<i64>, feat: &Feature) -> bool {
        let (
            Self::Worktree {
                call_files,
                literal_files,
                ..
            },
            Some(id),
        ) = (self, file_id)
        else {
            return true;
        };
        let held = |files: &HashMap<String, HashSet<i64>>, key: &str| {
            files.get(key).is_some_and(|ids| ids.contains(&id))
        };
        feat.call_name().is_none_or(|name| held(call_files, name))
            && feat
                .literal_keys()
                .iter()
                .all(|key| held(literal_files, key))
    }
}

/// Run the check over a resolved diff. Every file, changed or not, is read on
/// the diff's head side: the worktree for unstaged, the index for staged, the
/// commit tree otherwise. Enclosing symbols come from a parse of that side.
pub fn analyze(
    db: &Db,
    workspace_root: &Path,
    scope: &DiffScope,
    registry: &LanguageRegistry,
    controls: Controls,
    budget: Budget,
) -> Result<SiblingReport> {
    let file_hunks = git::git_diff_hunks(
        workspace_root,
        &scope.base_revision,
        scope.head_revision.as_deref(),
    )?;
    let mut parsers = Parsers::new();
    let mut report = SiblingReport::default();
    let mut patch = PatchHasher::default();
    let mut changed: Vec<Changed<'_>> = Vec::new();
    // Files changed on both sides, classified, awaiting the cross-file move
    // pass: `(index into changed, hunks, (old, new) path, signals)`.
    let mut pending: Vec<(usize, &git::FileHunks, (&str, &str), SymbolSignals)> = Vec::new();
    let (mut unmatched_old, mut unmatched_new) = (Vec::new(), Vec::new());
    // Head-side source of every file that may hold a survivor, for naming the
    // enclosing symbol.
    let mut sources: HashMap<&str, String> = HashMap::new();

    for fh in &file_hunks {
        let Some(path) = fh.new_path.as_deref().or(fh.old_path.as_deref()) else {
            continue;
        };
        let Some(adapter) = adapter_for_path(registry, path) else {
            continue;
        };
        if excluded_path(adapter, path) {
            continue;
        }
        let (old_src, new_src) = match read_sides(workspace_root, scope, fh) {
            Ok(sides) => sides,
            Err(e) => {
                report.incomplete.push(format!("{path}: {e}"));
                continue;
            }
        };
        patch.add_file(fh, old_src.as_deref(), new_src.as_deref());
        let old_path = fh.old_path.as_deref().unwrap_or(path);
        let new_path = fh.new_path.as_deref().unwrap_or(path);
        let lang = language_key(adapter);
        let mut tokens_of = |src: &Option<String>, p: &str| -> Option<FileTokens> {
            let src = src.as_deref()?;
            match parsers.tokens(adapter, src, p) {
                Ok(t) => Some(t),
                Err(e) => {
                    report.incomplete.push(format!("{p}: {e}"));
                    None
                }
            }
        };
        let old = tokens_of(&old_src, old_path);
        let new = tokens_of(&new_src, new_path);
        let added: HashSet<usize> = fh.hunks.iter().flat_map(|h| h.added_lines()).collect();

        let mut parse = |src: &Option<String>, p: &str| -> Option<ParseResult> {
            let src = src.as_deref()?;
            match parsers.parse(adapter, src, p) {
                Ok(parse) => Some(parse),
                Err(e) => {
                    report.incomplete.push(format!("{p}: {e}"));
                    None
                }
            }
        };
        let (old_parse, new_parse) = (parse(&old_src, old_path), parse(&new_src, new_path));
        match (&old_src, &new_src, &old_parse, &new_parse) {
            (Some(o), Some(n), Some(op), Some(np)) if old.is_some() && new.is_some() => {
                let (symbols, uo, un) =
                    SymbolSignals::classify((op, np), (o, n), (old_path, new_path));
                unmatched_old.extend(uo);
                unmatched_new.extend(un);
                pending.push((changed.len(), fh, (old_path, new_path), symbols));
            }
            // An added or deleted file: its symbols are move candidates.
            (Some(o), None, Some(op), _) => {
                unmatched_old.extend(build_unmatched(op, o, old_path));
            }
            (None, Some(n), _, Some(np)) => {
                unmatched_new.extend(build_unmatched(np, n, new_path));
            }
            _ => {}
        }
        if let (Some(p), Some(src)) = (fh.new_path.as_deref(), new_src) {
            sources.insert(p, src);
        }
        changed.push(Changed {
            new_path: fh.new_path.as_deref(),
            old,
            new,
            added,
            language: lang,
        });
    }
    report.patch = patch.finish();
    let mut moved = moved_spans(&unmatched_old, &unmatched_new);
    let mut signals = HunkSignals::default();
    for (i, fh, (old_path, new_path), mut symbols) in pending {
        let c = &changed[i];
        let (Some(old), Some(new)) = (&c.old, &c.new) else {
            continue;
        };
        symbols
            .kept_old
            .extend(moved.remove(old_path).unwrap_or_default());
        signals.collect(
            fh,
            (old_path, new_path),
            (old, new),
            &symbols,
            c.language,
            controls,
        );
    }
    let HunkSignals { removed, wraps, .. } = signals;
    if removed.is_empty() {
        return Ok(report);
    }

    let tree_scope = Scope::Tree {
        canonical_mapping: controls.canonical_mapping,
    };
    let mut occ = Occurrences::default();

    // The changed files, on both sides.
    for c in &changed {
        if let Some(old) = &c.old {
            for (key, lines) in tree_occurrences(&old.tokens, tree_scope, &removed, c.language) {
                *occ.pre_changed.entry(key).or_default() += lines.len();
            }
        }
        let (Some(new), Some(path)) = (&c.new, c.new_path) else {
            continue;
        };
        for (key, lines) in tree_occurrences(&new.tokens, tree_scope, &removed, c.language) {
            *occ.post_changed.entry(key).or_default() += lines.len();
            for l in lines {
                if c.added.contains(&l) {
                    occ.readded.entry(key).or_default().push((path, l));
                } else {
                    occ.survive(key, (path, l), &new.lines);
                }
            }
        }
    }

    // The rest of the tree, on the same side. Needles narrow which files can
    // hold an idiom (plus call refs when the index is the worktree),
    // tree-sitter confirms and locates it.
    let changed_paths: HashSet<&str> = changed.iter().filter_map(|c| c.new_path).collect();
    let (tree, mut reader) =
        SurvivorTree::open(db, workspace_root, scope, removed.keys().map(|(_, f)| f))?;
    let files = tree.files();
    let started = std::time::Instant::now();
    for (scanned, &(path, file_id)) in files.iter().enumerate() {
        if started.elapsed() >= budget.scan_time {
            report.incomplete.push(format!(
                "budget: survivor scan stopped after {scanned} of {} files ({} ms); \
                 survivors and counts in the rest are unknown",
                files.len(),
                budget.scan_time.as_millis()
            ));
            break;
        }
        if changed_paths.contains(path) {
            continue;
        }
        let Some(adapter) = adapter_for_path(registry, path) else {
            continue;
        };
        if excluded_path(adapter, path) {
            continue;
        }
        let lang = language_key(adapter);
        let wanted: Vec<&Key> = removed
            .keys()
            .filter(|(l, f)| *l == lang && tree.may_hold(file_id, f))
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let read = match reader.as_mut() {
            Some(r) => r.read(path),
            None => git::file_content_on_side(workspace_root, None, path),
        };
        let source = match read {
            Ok(Some(s)) => s,
            Ok(None) => continue, // listed but gone on this side: nothing survives there
            Err(e) => {
                report.incomplete.push(format!("{path}: {e}"));
                continue;
            }
        };
        let wanted: HashSet<&Key> = wanted
            .into_iter()
            .filter(|(_, f)| f.needles().iter().all(|n| source.contains(n)))
            .collect();
        if wanted.is_empty() {
            continue;
        }
        let toks = match parsers.tokens(adapter, &source, path) {
            Ok(t) => t,
            Err(e) => {
                report.incomplete.push(format!("{path}: {e}"));
                continue;
            }
        };
        let mut survived = false;
        for (key, lines) in tree_occurrences(&toks.tokens, tree_scope, &removed, lang) {
            if !wanted.contains(key) {
                continue;
            }
            *occ.unchanged.entry(key).or_default() += lines.len();
            for l in lines {
                occ.survive(key, (path, l), &toks.lines);
                survived = true;
            }
        }
        if survived {
            sources.insert(path, source);
        }
    }

    let items = classify(&removed, &mut occ, &wraps);
    let mut findings = group_litpairs(items);
    if controls.group_by_survivors {
        findings = group_by_survivors(findings);
    }
    findings.sort_by_key(|f| (f.class != PatternClass::Rewritten, f.survivors.len()));

    let mut symbols = SymbolLookup::default();
    for f in findings {
        let mut survivors = Vec::with_capacity(f.survivors.len());
        for site in f.survivors {
            let span = symbols.enclosing(
                &mut parsers,
                registry,
                &sources,
                site,
                &mut report.incomplete,
            );
            let snippet = occ.snippets.remove(&site).unwrap_or_default();
            let start = span.as_ref().map_or(1, |(start, _, _)| *start);
            let occurrence = sources
                .get(site.0)
                .map_or(0, |src| count_snippet(src, start..site.1, &snippet));
            survivors.push(Survivor {
                file: site.0.to_string(),
                line: site.1,
                symbol: span.map(|(_, _, name)| name),
                snippet,
                occurrence,
            });
        }
        report.findings.push(SiblingFinding {
            survivor_count: survivors.len(),
            survivors,
            idioms: f.idioms.into_iter().collect(),
            class: f.class,
            removed_at: f
                .removed_at
                .into_iter()
                .map(|(p, l)| format!("{p}:{l}"))
                .collect(),
        });
    }
    Ok(report)
}

/// Keep the removed idioms the diff rewrote or wrapped and that survive at a
/// plausible number of sites.
fn classify<'a>(
    removed: &'a BTreeMap<Key, Removed<'a>>,
    occ: &mut Occurrences<'a>,
    wraps: &HashSet<usize>,
) -> Vec<Item<'a>> {
    // An argfld (callee + field) is an idiom only when the handling of that
    // callee's result was rewritten too (`from_str(&x.col).unwrap_or_default()`
    // → a typed parse). Otherwise it is just another caller of f.
    let rewritten_heads: HashSet<&str> = removed
        .keys()
        .filter(|k| occ.rewritten(k))
        .filter_map(|(_, f)| match f {
            Feature::Chain { head, .. } => Some(head.as_str()),
            _ => None,
        })
        .collect();

    let mut items = Vec::new();
    for (key, rem) in removed {
        let feat = &key.1;
        if let Feature::Argfld { callee, .. } = feat
            && !rewritten_heads.contains(callee.as_str())
        {
            continue;
        }
        if rem.hunks.len() >= SWEEP_HUNKS {
            continue; // a sweep: its survivors are the canonical remainder
        }
        let sites = occ.survivors.remove(key).unwrap_or_default();
        if sites.is_empty() || sites.len() > MAX_SURVIVORS || occ.pre_df(key) > MAX_DF {
            continue;
        }
        let wrapped_in_place = occ.readded.get(key).is_some_and(|re| {
            re.iter().any(|(p, l)| {
                rem.origins
                    .iter()
                    .any(|(op, start)| op == p && l.abs_diff(*start) <= NEAR)
            })
        }) && rem.hunks.iter().any(|h| wraps.contains(h));
        let class = if occ.rewritten(key) {
            PatternClass::Rewritten
        } else if wrapped_in_place {
            PatternClass::Wrapped
        } else {
            continue; // moved: a move is not a fix
        };
        items.push(Item {
            feature: feat,
            class,
            removed_at: rem.sites.iter().copied().collect(),
            survivors: sites,
            hunks: &rem.hunks,
        });
    }
    items
}

fn line_text(lines: &[String], line: usize) -> String {
    line.checked_sub(1)
        .and_then(|i| lines.get(i))
        .map(|l| l.trim().to_string())
        .unwrap_or_default()
}

/// One reported idiom before grouping.
struct Item<'a> {
    feature: &'a Feature,
    class: PatternClass,
    removed_at: BTreeSet<Site<'a>>,
    survivors: BTreeSet<Site<'a>>,
    hunks: &'a BTreeSet<usize>,
}

/// A finding before its survivors get symbols.
struct Grouped<'a> {
    idioms: BTreeSet<Idiom>,
    class: PatternClass,
    removed_at: BTreeSet<Site<'a>>,
    survivors: BTreeSet<Site<'a>>,
}

impl<'a> Grouped<'a> {
    fn empty() -> Self {
        Self {
            idioms: BTreeSet::new(),
            class: PatternClass::Wrapped,
            removed_at: BTreeSet::new(),
            survivors: BTreeSet::new(),
        }
    }

    fn absorb(&mut self, other: Grouped<'a>) {
        self.idioms.extend(other.idioms);
        self.removed_at.extend(other.removed_at);
        self.survivors.extend(other.survivors);
        if other.class == PatternClass::Rewritten {
            self.class = PatternClass::Rewritten;
        }
    }
}

/// One finding per removed literal list: pairs removed from the same hunks
/// are a single list, not N² findings.
fn group_litpairs(items: Vec<Item<'_>>) -> Vec<Grouped<'_>> {
    let mut out = Vec::new();
    let mut lists: BTreeMap<&BTreeSet<usize>, (BTreeSet<&str>, Grouped<'_>)> = BTreeMap::new();
    for item in items {
        let mut grouped = Grouped {
            idioms: BTreeSet::new(),
            class: item.class,
            removed_at: item.removed_at,
            survivors: item.survivors,
        };
        let kind = match item.feature {
            Feature::Litpair(a, b) => {
                let (lits, list) = lists
                    .entry(item.hunks)
                    .or_insert_with(|| (BTreeSet::new(), Grouped::empty()));
                lits.insert(a);
                lits.insert(b);
                list.absorb(grouped);
                continue;
            }
            Feature::Sql(_) => IdiomKind::Sql,
            Feature::GrownList { .. } => IdiomKind::Litset,
            Feature::Chain { .. } => IdiomKind::Chain,
            Feature::Argfld { .. } => IdiomKind::Argfld,
        };
        grouped.idioms.insert(Idiom {
            kind,
            idiom: item.feature.text(),
        });
        out.push(grouped);
    }
    for (lits, mut list) in lists.into_values() {
        let joined: Vec<&str> = lits.into_iter().collect();
        list.idioms.insert(Idiom {
            kind: IdiomKind::Litset,
            idiom: format!("{{{}}}", joined.join(", ")),
        });
        out.push(list);
    }
    out
}

/// Findings that point at exactly the same survivors are one finding.
fn group_by_survivors(findings: Vec<Grouped<'_>>) -> Vec<Grouped<'_>> {
    let mut out: Vec<Grouped<'_>> = Vec::new();
    for f in findings {
        match out.iter_mut().find(|g| g.survivors == f.survivors) {
            Some(g) => g.absorb(f),
            None => out.push(f),
        }
    }
    out
}

/// Enclosing symbols from a parse of the reviewed side, one span list per
/// file: `(start_line, end_line, qualified_name)`.
#[derive(Default)]
struct SymbolLookup<'a> {
    files: HashMap<&'a str, Vec<SymbolSpan>>,
}

impl<'a> SymbolLookup<'a> {
    /// The innermost symbol holding the site: `(start_line, end_line, name)`.
    fn enclosing(
        &mut self,
        parsers: &mut Parsers,
        registry: &LanguageRegistry,
        sources: &HashMap<&str, String>,
        (path, line): Site<'a>,
        incomplete: &mut Vec<String>,
    ) -> Option<SymbolSpan> {
        let spans = self.files.entry(path).or_insert_with(|| {
            let Some(source) = sources.get(path) else {
                return Vec::new();
            };
            let Some(adapter) = adapter_for_path(registry, path) else {
                return Vec::new();
            };
            symbol_spans(&mut parsers.pool, adapter, source, path).unwrap_or_else(|e| {
                incomplete.push(format!("{path}: {e}"));
                Vec::new()
            })
        });
        spans
            .iter()
            .filter(|(start, end, _)| *start <= line && line <= *end)
            .min_by_key(|(start, end, _)| end - start)
            .cloned()
    }
}

// ---------------------------------------------------------------------------
// Firing log
// ---------------------------------------------------------------------------

/// The advisory as a review surface reports it: findings, or why there are
/// none. Shared by `sutra_review` and `sutra check`.
pub struct Advisory {
    pub report: SiblingReport,
    /// The check itself failed; `report` is empty and must not read as clean.
    pub error: Option<String>,
    /// The findings stand, but recording them in the firing log failed.
    pub firing_log_error: Option<String>,
}

impl Advisory {
    /// The review event's identity, or why the diff could not be hashed. The
    /// other mechanisms of the same review record under this event.
    pub fn patch(&self) -> std::result::Result<&ReviewedPatch, &str> {
        match &self.error {
            None => Ok(&self.report.patch),
            Some(e) => Err(e),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        let mut out = serde_json::json!({
            "advisory": true,
            "findings": self.report.findings,
        });
        if !self.report.incomplete.is_empty() {
            out["incomplete"] = serde_json::json!(self.report.incomplete);
        }
        if let Some(e) = &self.error {
            out["error"] = serde_json::json!(e);
        }
        if let Some(e) = &self.firing_log_error {
            out["firing_log_error"] = serde_json::json!(e);
        }
        out
    }
}

/// Run the check on `scope` and log what it flagged. Never fails the caller:
/// a failure is carried in the result so the surface can say so.
pub fn run_advisory(
    db: &Db,
    workspace_root: &Path,
    scope: &DiffScope,
    registry: &LanguageRegistry,
    surface: &str,
    diff_spec: &str,
) -> Advisory {
    match analyze(
        db,
        workspace_root,
        scope,
        registry,
        Controls::from_env(),
        Budget::from_env(),
    ) {
        Ok(report) => {
            let firing_log_error =
                record_firings(db, workspace_root, &report, surface, diff_spec, scope)
                    .err()
                    .map(|e| e.to_string());
            Advisory {
                report,
                error: None,
                firing_log_error,
            }
        }
        Err(e) => Advisory {
            report: SiblingReport::default(),
            error: Some(e.to_string()),
            firing_log_error: None,
        },
    }
}

/// Record one firing per survivor. Returns the number of new rows.
pub fn record_firings(
    db: &Db,
    workspace_root: &Path,
    report: &SiblingReport,
    surface: &str,
    diff_spec: &str,
    scope: &DiffScope,
) -> Result<usize> {
    if report.findings.is_empty() {
        return Ok(0);
    }
    let anchor = git::head_commit_hash(workspace_root);
    let ctx = FiringContext {
        surface,
        diff_spec,
        base_rev: Some(&scope.base_revision),
        head_rev: scope.head_revision.as_deref(),
        anchor_commit: anchor.as_deref(),
    };
    let event_id = crate::tools::firings::resolve_event(db, workspace_root, &ctx, &report.patch)?;
    let keys: Vec<(String, &str)> = report
        .findings
        .iter()
        .map(|f| {
            let idioms: Vec<&str> = f.idioms.iter().map(|i| i.idiom.as_str()).collect();
            let kind = f.idioms.first().map_or("", |i| i.kind.as_str());
            (idioms.join(" | "), kind)
        })
        .collect();
    let records: Vec<FiringRecord<'_>> = report
        .findings
        .iter()
        .zip(&keys)
        .flat_map(|(f, (key, kind))| {
            f.survivors.iter().map(move |s| FiringRecord {
                mechanism: MECHANISM,
                finding_kind: kind,
                finding_key: key,
                file_path: &s.file,
                line: Some(
                    i64::try_from(s.line).expect("invariant: a source line number fits in i64"),
                ),
                symbol: s.symbol.as_deref(),
                snippet: Some(&s.snippet),
                occurrence: i64::try_from(s.occurrence)
                    .expect("invariant: a line count fits in i64"),
            })
        })
        .collect();
    db.record_firings(event_id, &records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::adapter::default_registry;

    fn toks(lang: &str, src: &str) -> Vec<Token> {
        let registry = default_registry();
        let adapter = registry
            .adapter_for_language(lang)
            .expect("invariant: rust and dart adapters are registered");
        Parsers::new()
            .tokens(adapter, src, "src/x")
            .expect("fixture parses")
            .tokens
    }

    fn texts(lang: &str, src: &str, scope: Scope) -> Vec<String> {
        features(&toks(lang, src), scope)
            .keys()
            .map(Feature::text)
            .collect()
    }

    const TREE: Scope = Scope::Tree {
        canonical_mapping: true,
    };

    #[test]
    fn rust_chain_and_argfld() {
        let src = "fn f(t: &T) -> Vec<String> {\n    serde_json::from_str::<Vec<String>>(&t.refs).unwrap_or_default()\n}\n";
        let got = texts("rust", src, Scope::Removed);
        assert!(
            got.contains(&"serde_json::from_str.unwrap_or_default".to_string()),
            "{got:?}"
        );
        assert!(
            got.contains(&"serde_json::from_str(.refs)".to_string()),
            "{got:?}"
        );
    }

    #[test]
    fn generic_pairs_are_not_idioms() {
        let got = texts(
            "rust",
            "fn f(v: &[u8]) { v.iter().map(|x| x).count(); }\n",
            Scope::Removed,
        );
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn comments_macros_and_raw_strings_are_not_confused() {
        let src = "fn f() {\n    // a(\"x\").b()\n    println!(\"{}\", 1);\n    let s = r#\"a \"quoted\" (paren\"#;\n    helper(1).finish();\n}\n";
        let got = texts("rust", src, Scope::Removed);
        assert_eq!(got, vec!["helper.finish".to_string()]);
    }

    #[test]
    fn literal_pairs_skip_keys_and_templates() {
        let src = "fn f(m: &mut M) {\n    let langs = [\"dart\", \"rust\"];\n    m.insert(\"key\".into(), \"{x}\");\n    let j = json!({\"from\": 1, \"to\": 2});\n}\n";
        let got = texts("rust", src, Scope::Removed);
        assert_eq!(got, vec!["\"dart\"+\"rust\"".to_string()]);
    }

    #[test]
    fn canonical_mapping_literals_drop_from_the_tree_only() {
        let src = "fn f(s: S) -> &'static str {\n    match s {\n        S::Done => \"done\",\n        S::Wontfix => \"wontfix\",\n    }\n}\n";
        assert!(texts("rust", src, TREE).is_empty());
        assert_eq!(
            texts("rust", src, Scope::Removed),
            vec!["\"done\"+\"wontfix\"".to_string()]
        );
        // An if-chain returning literals is not a mapping arm (guard.rs:407).
        let chain = "fn g(e: &str) -> Option<&str> {\n    if e == \"rs\" { Some(\"rust\") } else if e == \"dart\" { Some(\"dart\") } else { None }\n}\n";
        assert!(!texts("rust", chain, TREE).is_empty());
    }

    #[test]
    fn sql_prefix_normalizes_line_continuations() {
        let src =
            "fn f() { let q = \"SELECT id, constraint_id, \\\n     constraint_name FROM t\"; }\n";
        let got = texts("rust", src, Scope::Removed);
        assert_eq!(
            got,
            vec!["\"SELECT id, constraint_id, constraint_na".to_string()]
        );
    }

    #[test]
    fn test_items_are_not_tokens() {
        let src = "fn f() { a(1).b(); }\n#[cfg(test)]\nmod tests {\n    fn t() { c(1).d(); }\n}\n";
        let got = texts("rust", src, TREE);
        assert_eq!(got, vec!["a.b".to_string()]);
    }

    #[test]
    fn dart_chain_argfld_and_literals() {
        let src = "List<String> f(Task t) {\n  final kinds = ['draft', 'final'];\n  return jsonDecode(t.refs).map((e) => e as String).toList();\n}\n";
        let got = texts("dart", src, Scope::Removed);
        assert!(got.contains(&"jsonDecode.map".to_string()), "{got:?}");
        assert!(got.contains(&"jsonDecode(.refs)".to_string()), "{got:?}");
        assert!(got.contains(&"'draft'+'final'".to_string()), "{got:?}");
    }

    #[test]
    fn dart_null_aware_chain() {
        let got = texts(
            "dart",
            "void f(A a) { load(a)?.render(); }\n",
            Scope::Removed,
        );
        assert!(got.contains(&"load.render".to_string()), "{got:?}");
    }

    #[test]
    fn dart_enum_mapping_is_canonical() {
        let src = "String f(Kind k) => switch (k) {\n  Kind.draft => 'draft',\n  Kind.done => 'done',\n};\n";
        assert!(
            texts("dart", src, TREE).is_empty(),
            "{:?}",
            texts("dart", src, TREE)
        );
    }

    #[test]
    fn grouping_merges_lists_and_shared_survivors() {
        let hunks: BTreeSet<usize> = [1].into_iter().collect();
        let (a, b, c) = (
            Feature::Litpair("\"a\"".into(), "\"b\"".into()),
            Feature::Litpair("\"b\"".into(), "\"c\"".into()),
            Feature::Chain {
                head: "f".into(),
                method: "g".into(),
            },
        );
        let site: BTreeSet<Site<'_>> = [("src/y.rs", 3)].into_iter().collect();
        let item = |feature, class| Item {
            feature,
            class,
            removed_at: BTreeSet::new(),
            survivors: site.iter().copied().collect(),
            hunks: &hunks,
        };
        let grouped = group_litpairs(vec![
            item(&a, PatternClass::Wrapped),
            item(&b, PatternClass::Rewritten),
            item(&c, PatternClass::Wrapped),
        ]);
        assert_eq!(grouped.len(), 2);
        let list = grouped
            .iter()
            .find(|g| g.idioms.iter().any(|i| i.kind == IdiomKind::Litset))
            .expect("one list");
        assert_eq!(list.class, PatternClass::Rewritten);
        assert_eq!(
            list.idioms.first().map(|i| i.idiom.as_str()),
            Some("{\"a\", \"b\", \"c\"}")
        );
        let merged = group_by_survivors(grouped);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].idioms.len(), 2);
    }
}
