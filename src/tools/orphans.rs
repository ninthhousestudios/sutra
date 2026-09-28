//! "Nothing calls this": the orphans advisory (sutra/483), for the UNWIRED
//! failure mode (built ahead of its call site, never connected).
//!
//! Two shapes, both measured in `docs/orphans-backtest.md`:
//!
//! - `added`: a symbol the diff adds that no non-test code references.
//! - `orphaned`: a symbol the diff leaves unreferenced by removing its last
//!   non-test reference. Two of the three reachable UNWIRED incidents took this
//!   shape (sutra/297, adityas/ai/110): the symbol had a caller when written,
//!   and a later change deleted it.
//!
//! Advisory, never gating: building ahead of a call site inside a multi-commit
//! task is legitimate, so a finding names the symbol and says nothing calls it
//! yet. Liveness is read from the index, which the review surfaces refresh to
//! the worktree first, so the check runs only when the reviewed side is the
//! worktree, the index or HEAD; a changed file whose reviewed content differs
//! from what was indexed, or an indexed file outside the diff that differs
//! from the reviewed side, makes the result `incomplete`, never clean.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use serde::Serialize;
use serde_json::json;

use crate::db::Db;
use crate::db::firings::{FiringContext, FiringRecord};
use crate::db::orphans::{Liveness, SymbolSite};
use crate::error::Result;
use crate::git;
use crate::parser::adapter::{LanguageRegistry, ParserPool};
use crate::parser::{self, ParseResult, flatten_symbols};
use crate::tools::advisory::{self, adapter_for, dirty_outside_diff, index_mismatch};
use crate::tools::firings::ReviewedPatch;
use crate::tools::review::DiffScope;
use crate::tools::sibling_pattern::read_sides;
use crate::tools::symbol_diff::{build_unmatched, classify_symbols, resolve_renames};

/// The mechanism name in the firing log.
pub const MECHANISM: &str = "orphan";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanKind {
    /// The diff added it and nothing outside tests references it.
    Added,
    /// The diff removed its last reference outside tests.
    Orphaned,
}

impl OrphanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Orphaned => "orphaned",
        }
    }

    fn detail(self) -> &'static str {
        match self {
            Self::Added => "added by this change; nothing outside tests references it yet",
            Self::Orphaned => "this change removed its last reference outside tests",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Orphan {
    pub kind: OrphanKind,
    pub symbol: String,
    pub symbol_kind: String,
    pub file: String,
    pub line: i64,
    /// References from test code: exercised, never called.
    pub test_refs: usize,
}

impl Orphan {
    fn new(kind: OrphanKind, sym: SymbolSite, test_refs: usize) -> Self {
        Self {
            kind,
            symbol: sym.qualified_name,
            symbol_kind: sym.kind,
            file: sym.path,
            line: sym.start_line,
            test_refs,
        }
    }
}

#[derive(Debug, Default)]
pub struct OrphanReport {
    pub findings: Vec<Orphan>,
    /// Why the result may be missing findings. Non-empty means incomplete,
    /// never clean.
    pub incomplete: Vec<String>,
}

/// Whether a finding may name `sym`: a kind `sutra_dead` reports, outside
/// test code, not an entrypoint.
fn reportable(sym: &SymbolSite, is_test_path: &impl Fn(&str) -> bool) -> bool {
    crate::db::DEAD_CODE_KINDS.contains(&sym.kind.as_str())
        && sym.short_name != "main"
        && !is_test_path(&sym.path)
        // Test items, FFI and framework entrypoints, trait impls / overrides.
        && sym.flags & 15 == 0
        // The Dart adapter emits read refs only for private names (sutra/288),
        // so a public top-level or static variable read (`ref.watch(fooProvider)`,
        // `Xsd.xdouble`) never binds and every such variable reads as
        // unreferenced: 18 of 25 Dart items in the back-test before this rule.
        // Private (`_`-prefixed) reads are extracted, so those stay reportable.
        // Lift this once the adapter extracts the public reads (sutra/497).
        && !(sym.language == "dart"
            && matches!(sym.kind.as_str(), "const" | "static")
            && !sym.short_name.starts_with('_'))
}

/// Unreferenced outside tests, and not test support: a helper whose name says
/// it is for tests (`Config::test_default`, `Db::conn_for_test`) and that only
/// tests reference is doing its job.
fn unwired(sym: &SymbolSite, liveness: Liveness) -> bool {
    !liveness.live
        && (liveness.test_refs == 0 || !sym.short_name.to_ascii_lowercase().contains("test"))
}

/// A reference the diff removed from non-test code: `(name, qualifier)`.
type RemovedRef = (String, Option<String>);

/// Whether a removed reference could have bound to `sym`: its qualifier's last
/// segment names `sym`'s type or module file. A reference whose qualifier
/// picks no type or module (none, `self`, `crate`) fits only when `sym` is the
/// one definition of its name (`unique`): with no parse of the base tree,
/// removing `helper()` says nothing about which of two `helper`s it called,
/// and an already-dead one would be reported as orphaned by this change.
pub(crate) fn qualifier_fits(
    qualifier: Option<&str>,
    (qualified_name, path): (&str, &str),
    unique: bool,
) -> bool {
    let Some(q) = qualifier else {
        return unique;
    };
    let last = q.rsplit("::").next().unwrap_or(q);
    if matches!(last, "self" | "Self" | "super" | "crate") {
        return unique;
    }
    let parent = qualified_name
        .rsplit_once("::")
        .map(|(p, _)| p.rsplit("::").next().unwrap_or(p));
    let stem = Path::new(path).file_stem().and_then(|s| s.to_str());
    parent == Some(last) || stem == Some(last)
}

/// The references on `ranges` of a file's old side, outside its test items.
fn removed_refs(parse: ParseResult, ranges: &[std::ops::Range<usize>], out: &mut Vec<RemovedRef>) {
    let test_spans: Vec<(usize, usize)> = flatten_symbols(&parse.symbols)
        .into_iter()
        .filter(|s| parser::flags_mark_test(i64::from(s.flags), &parse.language))
        .map(|s| (s.start_line, s.end_line))
        .collect();
    out.extend(
        parse
            .references
            .into_iter()
            .filter(|r| {
                ranges.iter().any(|range| range.contains(&r.line))
                    && !test_spans.iter().any(|(a, b)| (*a..=*b).contains(&r.line))
            })
            .map(|r| (r.name, r.qualifier)),
    );
}

/// Find the orphans of `scope`. [`run_advisory`] first checks that the index
/// can stand for the reviewed side.
pub fn analyze(
    db: &Db,
    workspace_root: &Path,
    scope: &DiffScope,
    registry: &LanguageRegistry,
) -> Result<OrphanReport> {
    let is_test_path = |p: &str| adapter_for(registry, p).is_some_and(|a| a.is_test_path(p));
    let file_hunks = git::git_diff_hunks(
        workspace_root,
        &scope.base_revision,
        scope.head_revision.as_deref(),
    )?;
    let mut pool = ParserPool::new(std::time::Duration::from_secs(5));
    let mut report = OrphanReport::default();
    report
        .incomplete
        .extend(dirty_outside_diff(db, workspace_root, scope, registry)?);
    let (mut unmatched_old, mut unmatched_new) = (Vec::new(), Vec::new());
    let mut removed: Vec<RemovedRef> = Vec::new();

    for fh in &file_hunks {
        let Some(path) = fh.new_path.as_deref().or(fh.old_path.as_deref()) else {
            continue;
        };
        let Some(adapter) = adapter_for(registry, path) else {
            continue;
        };
        if !db.indexes_language(adapter.language_id())? {
            continue;
        }
        if let Some(new_path) = fh.new_path.as_deref()
            && let Some(why) = index_mismatch(db, workspace_root, scope, new_path)
        {
            report.incomplete.push(why);
        }
        let (old_src, new_src) = match read_sides(workspace_root, scope, fh) {
            Ok(sides) => sides,
            Err(e) => {
                report.incomplete.push(format!("{path}: {e}"));
                continue;
            }
        };
        let old_path = fh.old_path.as_deref().unwrap_or(path);
        let new_path = fh.new_path.as_deref().unwrap_or(path);
        let mut parse = |src: Option<&str>, p: &str| -> Option<ParseResult> {
            match pool.parse_with(adapter, src?, p) {
                Ok(parse) => Some(parse),
                Err(e) => {
                    report.incomplete.push(format!("{p}: {e}"));
                    None
                }
            }
        };
        let old = parse(old_src.as_deref(), old_path);
        let new = parse(new_src.as_deref(), new_path);
        match (&old_src, &new_src, &old, &new) {
            (Some(o), Some(n), Some(op), Some(np)) => {
                let result = classify_symbols(op, np, o, n, old_path, new_path);
                unmatched_old.extend(result.unmatched_old);
                unmatched_new.extend(result.unmatched_new);
            }
            (Some(o), None, Some(op), _) => unmatched_old.extend(build_unmatched(op, o, old_path)),
            (None, Some(n), _, Some(np)) => unmatched_new.extend(build_unmatched(np, n, new_path)),
            _ => {}
        }
        if let Some(op) = old
            && !is_test_path(old_path)
        {
            // A deleted file removed every line.
            let ranges: Vec<_> = match fh.new_path {
                Some(_) => fh.hunks.iter().map(git::Hunk::removed_lines).collect(),
                None => std::iter::once(1..usize::MAX).collect(),
            };
            removed_refs(op, &ranges, &mut removed);
        }
    }

    // Renamed or moved symbols are not new.
    let moved = resolve_renames(&unmatched_old, &unmatched_new).matched_new;
    let added_keys: BTreeSet<(&str, &str, &str)> = unmatched_new
        .iter()
        .enumerate()
        .filter(|(i, _)| !moved.contains(i))
        .map(|(_, s)| (s.file.as_str(), s.qualified_name.as_str(), s.kind.as_str()))
        .collect();

    let mut reported: HashSet<i64> = HashSet::new();
    let mut added_ids: HashSet<i64> = HashSet::new();
    for &(file, qualified_name, kind) in &added_keys {
        let sites = db.symbols_defined_as(file, qualified_name, kind)?;
        added_ids.extend(sites.iter().map(|s| s.id));
        // One finding per definition; twins share liveness.
        let Some(first) = sites.into_iter().next() else {
            continue;
        };
        if !reportable(&first, &is_test_path) {
            continue;
        }
        let liveness = db.production_liveness(&first, is_test_path)?;
        if unwired(&first, liveness) && reported.insert(first.id) {
            report
                .findings
                .push(Orphan::new(OrphanKind::Added, first, liveness.test_refs));
        }
    }

    let mut by_name: BTreeMap<&str, Vec<Option<&str>>> = BTreeMap::new();
    for (name, qualifier) in &removed {
        by_name
            .entry(name.as_str())
            .or_default()
            .push(qualifier.as_deref());
    }
    for (name, qualifiers) in by_name {
        let candidates = db.symbols_named(name)?;
        // Twins (same file and qualified name) are one definition.
        let definitions: HashSet<(&str, &str)> = candidates
            .iter()
            .map(|s| (s.path.as_str(), s.qualified_name.as_str()))
            .collect();
        let unique = definitions.len() == 1;
        for sym in candidates {
            if added_ids.contains(&sym.id)
                || reported.contains(&sym.id)
                || !reportable(&sym, &is_test_path)
                || !qualifiers
                    .iter()
                    .any(|q| qualifier_fits(*q, (&sym.qualified_name, &sym.path), unique))
            {
                continue;
            }
            let liveness = db.production_liveness(&sym, is_test_path)?;
            if unwired(&sym, liveness) {
                reported.insert(sym.id);
                report
                    .findings
                    .push(Orphan::new(OrphanKind::Orphaned, sym, liveness.test_refs));
            }
        }
    }
    report
        .findings
        .sort_by(|a, b| (a.kind, &a.file, a.line).cmp(&(b.kind, &b.file, b.line)));
    Ok(report)
}

/// The advisory as a review surface reports it: findings, or why there are
/// none. Shared by `sutra_review` and `sutra check`.
#[derive(Debug, Default)]
pub struct Advisory {
    pub report: OrphanReport,
    /// The check did not run, and why: the index does not hold the reviewed side.
    pub skipped: Option<String>,
    /// The check itself failed; `report` is empty and must not read as clean.
    pub error: Option<String>,
    /// The findings stand, but recording them in the firing log failed.
    pub firing_log_error: Option<String>,
}

impl Advisory {
    /// Findings grouped by kind and file.
    pub fn to_json(&self) -> serde_json::Value {
        let mut groups: BTreeMap<(OrphanKind, &str), Vec<&Orphan>> = BTreeMap::new();
        for f in &self.report.findings {
            groups.entry((f.kind, &f.file)).or_default().push(f);
        }
        let findings: Vec<_> = groups
            .into_iter()
            .map(|((kind, file), items)| {
                json!({
                    "kind": kind,
                    "file": file,
                    "detail": kind.detail(),
                    "symbols": items.iter().map(|o| json!({
                        "symbol": o.symbol,
                        "kind": o.symbol_kind,
                        "line": o.line,
                        "test_refs": o.test_refs,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let mut out = json!({ "advisory": true, "findings": findings });
        if !self.report.incomplete.is_empty() {
            out["incomplete"] = json!(self.report.incomplete);
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
        if let Some(e) = &self.error {
            let _ = writeln!(out, "\norphans check failed (not gating): {e}");
        }
        if let Some(s) = &self.skipped {
            let _ = writeln!(out, "\norphans check skipped: {s}");
        }
        let findings = &self.report.findings;
        if !findings.is_empty() {
            let _ = writeln!(
                out,
                "\n{} symbol(s) nothing outside tests references (advisory, not gating):",
                findings.len()
            );
        }
        for f in findings {
            let tests = match f.test_refs {
                0 => String::new(),
                n => format!(", {n} test ref(s)"),
            };
            let _ = writeln!(
                out,
                "  [{}] {}:{}  {} ({}{tests})",
                f.kind.as_str(),
                f.file,
                f.line,
                f.symbol,
                f.symbol_kind
            );
        }
        if !self.report.incomplete.is_empty() {
            let _ = writeln!(
                out,
                "  orphans check incomplete: {}",
                self.report.incomplete.join("; ")
            );
        }
        if let Some(e) = &self.firing_log_error {
            let _ = writeln!(out, "  (firing log not written: {e})");
        }
    }
}

/// Run the check on `scope` and log what it flagged under the review event
/// `patch` identifies: the one the sibling check resolved, or why it could
/// not hash the diff. Never fails the caller: a failure is carried in the
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

/// Record one firing per finding. The declaration line is the site, so the
/// acted-on proxy reads `changed` when the symbol is deleted or its
/// declaration rewritten, and `present` when it is wired up.
fn record_firings(
    db: &Db,
    workspace_root: &Path,
    report: &OrphanReport,
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
        report.findings.iter().map(|f| (f.file.as_str(), f.line)),
    )?;
    let records: Vec<FiringRecord<'_>> = report
        .findings
        .iter()
        .zip(&snippets)
        .map(|(f, snippet)| FiringRecord {
            mechanism: MECHANISM,
            finding_kind: f.kind.as_str(),
            finding_key: &f.symbol,
            file_path: &f.file,
            line: Some(f.line),
            symbol: Some(&f.symbol),
            snippet: Some(snippet),
            occurrence: 0,
        })
        .collect();
    db.record_firings(event_id, &records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site(qualified_name: &str, path: &str) -> SymbolSite {
        SymbolSite {
            id: 1,
            qualified_name: qualified_name.to_string(),
            short_name: qualified_name
                .rsplit("::")
                .next()
                .unwrap_or(qualified_name)
                .to_string(),
            kind: "function".to_string(),
            path: path.to_string(),
            language: "rust".to_string(),
            start_line: 1,
            flags: 0,
        }
    }

    #[test]
    fn qualifier_names_the_type_or_the_module_file() {
        let method = site("Telemetry::start", "src/ai_metrics.rs");
        assert!(qualifier_fits(
            Some("Telemetry"),
            (&method.qualified_name, &method.path),
            false
        ));
        assert!(qualifier_fits(
            Some("crate::ai_metrics::Telemetry"),
            (&method.qualified_name, &method.path),
            false
        ));
        assert!(!qualifier_fits(
            Some("Pricing"),
            (&method.qualified_name, &method.path),
            true
        ));
        let free = site("handle", "src/tools/dead.rs");
        assert!(qualifier_fits(
            Some("tools::dead"),
            (&free.qualified_name, &free.path),
            false
        ));
        assert!(!qualifier_fits(
            Some("tools::resolve"),
            (&free.qualified_name, &free.path),
            true
        ));
    }

    #[test]
    fn an_unscoped_qualifier_fits_only_the_one_definition() {
        let method = site("Telemetry::start", "src/ai_metrics.rs");
        for q in [None, Some("Self"), Some("crate"), Some("self")] {
            assert!(
                qualifier_fits(q, (&method.qualified_name, &method.path), true),
                "{q:?}"
            );
            assert!(
                !qualifier_fits(q, (&method.qualified_name, &method.path), false),
                "{q:?}"
            );
        }
    }

    #[test]
    fn test_support_is_not_unwired_while_tests_use_it() {
        let tested = Liveness {
            live: false,
            test_refs: 2,
        };
        let unused = Liveness {
            live: false,
            test_refs: 0,
        };
        assert!(!unwired(
            &site("Config::test_default", "src/config.rs"),
            tested
        ));
        assert!(!unwired(&site("Db::conn_for_test", "src/db.rs"), tested));
        assert!(unwired(&site("Db::conn_for_test", "src/db.rs"), unused));
        assert!(unwired(
            &site("SmritiReader::read_cursor", "src/smriti.rs"),
            tested
        ));
    }

    #[test]
    fn dart_variables_and_entrypoints_are_not_reportable() {
        let never_test = |_: &str| false;
        let mut v = site("fooProvider", "lib/a.dart");
        v.language = "dart".to_string();
        v.kind = "const".to_string();
        assert!(!reportable(&v, &never_test));
        let mut c = site("LIMIT", "src/a.rs");
        c.kind = "const".to_string();
        assert!(reportable(&c, &never_test));
        let mut ffi = site("exported", "src/a.rs");
        ffi.flags = 0x04;
        assert!(!reportable(&ffi, &never_test));
        assert!(!reportable(&site("main", "src/main.rs"), &never_test));
    }
}
