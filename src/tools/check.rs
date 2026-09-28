//! `sutra check` — a scriptable constraint gate for git hooks and CI.
//!
//! Reuses the same evaluation core as `sutra_review` (`review::build_findings`
//! → `constraints::check::evaluate`), scoped by a diff mode, and reduces the
//! result to a pass/fail decision plus a renderable report. The MCP review
//! surface computes exactly these `constraint_violations` /
//! `waived_constraint_violations`; this module exposes the same evaluation
//! through a CLI with a proper exit code so a pre-commit hook or CI job can
//! block an unwaived violation that never routed through the edit-time guard.

use std::path::Path;

use serde_json::json;

use crate::constraints::ConstraintFinding;
use crate::constraints::check::DiffHead;
use crate::error::Result;
use crate::parser::adapter::LanguageRegistry;
use crate::rules::{ConstraintParseError, Severity};
use crate::tools::{dup_exists, orphans, review, sibling_pattern};
use crate::waivers::Waived;

/// Outcome of a `sutra check` run over a diff scope.
pub struct CheckReport {
    /// Active, unwaived violations at or above the severity threshold — these
    /// fail the gate.
    pub blocking: Vec<ConstraintFinding>,
    /// Active, unwaived violations below the threshold — reported for context,
    /// never gating.
    pub below_threshold: Vec<ConstraintFinding>,
    /// Violations suppressed by a waiver in `.sutra/accepted.toml`, surfaced so
    /// a waived match stays visible rather than silently absent.
    pub waived: Vec<Waived<ConstraintFinding>>,
    /// Malformed `[[constraint]]` blocks. A rule set that fails to parse is
    /// treated as blocking — a silently-inert gate is worse than a loud one.
    pub parse_errors: Vec<ConstraintParseError>,
    pub diff_mode: String,
    pub threshold: Severity,
    pub scanned_files: usize,
    /// The "you fixed 1 of N" advisory (sutra/467). Reported, never gating.
    pub sibling_patterns: Option<sibling_pattern::Advisory>,
    /// The orphans advisory (sutra/483): symbols nothing outside tests
    /// references. Reported, never gating.
    pub orphans: Option<orphans::Advisory>,
    /// The "this already exists" advisory (sutra/469): added code that
    /// resembles an existing function. Reported, never gating.
    pub dup_exists: Option<dup_exists::Advisory>,
    /// Why the constraint findings were not recorded in the firing log, if
    /// they were not (sutra/486). Reported, never gating.
    pub firing_log_error: Option<String>,
}

impl CheckReport {
    /// The gate fails if any violation meets the threshold, or the rule set
    /// itself is malformed (an unparseable rule can't be trusted to be clean).
    pub fn failed(&self) -> bool {
        !self.blocking.is_empty() || !self.parse_errors.is_empty()
    }
}

/// Evaluate workspace constraints over the files in `diff_mode` and partition
/// the active violations by the severity threshold. Reads the current index;
/// forbidden-pattern checks read file content from disk, so violations in
/// already-indexed files are caught against live content.
pub fn handle(
    db: &crate::db::Db,
    workspace_root: &Path,
    diff_mode: &str,
    threshold: Severity,
    registry: &LanguageRegistry,
) -> Result<CheckReport> {
    let scope = review::resolve_diff_entries(workspace_root, diff_mode)?;
    let changed_paths = scope.paths();
    let base_revision = &scope.base_revision;
    // Gate the *requested snapshot*, not the working tree: the staged index, a
    // commit, or the worktree only for unstaged. Reading disk instead would let
    // a fix applied only in the worktree mask still-staged bytes, and vice
    // versa (sutra/385).
    let content = scope.content();
    let added_lines = review::diff_added_lines(workspace_root, base_revision, content)?;
    let findings = review::build_findings(
        db,
        workspace_root,
        &changed_paths,
        base_revision,
        DiffHead {
            content,
            added_lines: &added_lines,
        },
        None,
        registry,
    )?;

    let sibling_patterns =
        sibling_pattern::run_advisory(db, workspace_root, &scope, registry, "check", diff_mode);
    let orphans = orphans::run_advisory(
        db,
        workspace_root,
        &scope,
        registry,
        ("check", diff_mode),
        sibling_patterns.patch(),
    );
    let dup_exists = dup_exists::run_advisory(
        db,
        workspace_root,
        &scope,
        registry,
        ("check", diff_mode),
        sibling_patterns.patch(),
    );
    let firing_log_error = review::record_constraint_firings(
        db,
        workspace_root,
        &scope,
        review::FiringSurface {
            surface: "check",
            diff_spec: diff_mode,
            content,
        },
        &sibling_patterns,
        &findings.constraint_violations,
        registry,
    );

    let (blocking, below_threshold): (Vec<_>, Vec<_>) = findings
        .constraint_violations
        .into_iter()
        .partition(|f| f.severity.ordinal() >= threshold.ordinal());

    Ok(CheckReport {
        blocking,
        below_threshold,
        waived: findings.waived_constraint_violations,
        parse_errors: findings.constraint_parse_errors,
        diff_mode: diff_mode.to_string(),
        threshold,
        scanned_files: changed_paths.len(),
        sibling_patterns: Some(sibling_patterns),
        orphans: Some(orphans),
        dup_exists: Some(dup_exists),
        firing_log_error,
    })
}

/// Format one finding as `path:line  [severity] name (kind)` plus detail,
/// snippet, and provenance lines. `line`/`snippet` are omitted when absent.
fn render_finding(f: &ConstraintFinding, out: &mut String) {
    use std::fmt::Write;

    let location = match (f.line, f.from_path.is_empty()) {
        (Some(line), _) => format!("{}:{}", f.from_path, line),
        (None, false) => f.from_path.as_str().to_string(),
        (None, true) => "<workspace>".to_string(),
    };
    let name = f
        .constraint_name
        .as_deref()
        .unwrap_or(f.constraint_id.as_ref());
    let _ = writeln!(
        out,
        "  {location}  [{}] {name} ({})",
        f.severity.as_str(),
        f.constraint_kind,
    );
    let _ = writeln!(out, "      {}", f.detail);
    if let Some(snippet) = &f.snippet {
        let _ = writeln!(out, "      | {}", snippet.trim_end());
    }
    if let Some(prov) = &f.provenance {
        let _ = writeln!(out, "      ({prov})");
    }
}

/// Human-readable report. Blocking violations first, then a summary line;
/// below-threshold and waived findings are noted but never gate.
pub fn render_human(report: &CheckReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    if !report.parse_errors.is_empty() {
        let _ = writeln!(
            out,
            "malformed rules ({} — blocking):",
            report.parse_errors.len()
        );
        for e in &report.parse_errors {
            let name = e
                .name
                .as_deref()
                .map(|n| format!(" (name: {n})"))
                .unwrap_or_default();
            let _ = writeln!(out, "  constraint #{}{}: {}", e.index, name, e.error);
        }
    }

    if report.blocking.is_empty() && report.parse_errors.is_empty() {
        let _ = writeln!(
            out,
            "check passed: no unwaived {} violations in {} changed file(s) [{}]",
            report.threshold.as_str(),
            report.scanned_files,
            report.diff_mode,
        );
    } else if !report.blocking.is_empty() {
        let _ = writeln!(
            out,
            "check failed: {} violation(s) at or above '{}' [{}]:",
            report.blocking.len(),
            report.threshold.as_str(),
            report.diff_mode,
        );
        for f in &report.blocking {
            render_finding(f, &mut out);
        }
    }

    if !report.below_threshold.is_empty() {
        let _ = writeln!(
            out,
            "\n{} finding(s) below threshold (not gating):",
            report.below_threshold.len(),
        );
        for f in &report.below_threshold {
            render_finding(f, &mut out);
        }
    }
    if !report.waived.is_empty() {
        let _ = writeln!(out, "\n{} waived violation(s).", report.waived.len());
    }
    let justified: Vec<_> = review::justified(&report.waived).collect();
    if !justified.is_empty() {
        let _ = writeln!(
            out,
            "\n{} match(es) justified in place by this diff (not gating):",
            justified.len()
        );
        for w in justified {
            let f = &w.finding;
            let name = f
                .constraint_name
                .as_deref()
                .unwrap_or(f.constraint_id.as_ref());
            let line = f.line.map(|l| format!(":{l}")).unwrap_or_default();
            let _ = writeln!(out, "  {}{line}  {name}: {}", f.from_path, w.rationale);
        }
    }
    if let Some(advisory) = &report.sibling_patterns {
        render_sibling_patterns(advisory, &mut out);
    }
    if let Some(advisory) = &report.orphans {
        advisory.render(&mut out);
    }
    if let Some(advisory) = &report.dup_exists {
        advisory.render(&mut out);
    }
    if let Some(e) = &report.firing_log_error {
        let _ = writeln!(out, "\n(constraint firings not logged: {e})");
    }

    out
}

/// The sibling-pattern advisory: each idiom the diff removed, and where it
/// still survives. Advisory only.
fn render_sibling_patterns(advisory: &sibling_pattern::Advisory, out: &mut String) {
    use std::fmt::Write;
    if let Some(e) = &advisory.error {
        let _ = writeln!(out, "\nsibling-pattern check failed (not gating): {e}");
    }
    let findings = &advisory.report.findings;
    if !findings.is_empty() {
        let _ = writeln!(
            out,
            "\n{} idiom(s) this diff removed survive elsewhere (advisory, not gating):",
            findings.len()
        );
    }
    for f in findings {
        let idioms: Vec<&str> = f.idioms.iter().map(|i| i.idiom.as_str()).collect();
        let kind = f.idioms.first().map_or("", |i| i.kind.as_str());
        let class = match f.class {
            sibling_pattern::PatternClass::Rewritten => "rewritten",
            sibling_pattern::PatternClass::Wrapped => "wrapped",
        };
        let _ = writeln!(out, "  [{class} {kind}] {}", idioms.join("  |  "));
        let _ = writeln!(out, "      removed at {}", f.removed_at.join(", "));
        for s in &f.survivors {
            match &s.symbol {
                Some(sym) => {
                    let _ = writeln!(out, "      survives {}:{}  ({sym})", s.file, s.line);
                }
                None => {
                    let _ = writeln!(out, "      survives {}:{}", s.file, s.line);
                }
            }
        }
    }
    if !advisory.report.incomplete.is_empty() {
        let _ = writeln!(
            out,
            "  sibling-pattern check incomplete, {} file(s) unread: {}",
            advisory.report.incomplete.len(),
            advisory.report.incomplete.join("; ")
        );
    }
    if let Some(e) = &advisory.firing_log_error {
        let _ = writeln!(out, "  (firing log not written: {e})");
    }
}

fn finding_json(f: &ConstraintFinding) -> serde_json::Value {
    let mut entry = json!({
        "constraint_id": f.constraint_id,
        "constraint_name": f.constraint_name,
        "kind": f.constraint_kind,
        "severity": f.severity.as_str(),
        "provenance": f.provenance,
        "from": f.from_path,
        "to": f.to_path,
        "component_context": f.component_context,
        "detail": f.detail,
    });
    if let Some(line) = f.line {
        entry["line"] = json!(line);
    }
    if let Some(snippet) = &f.snippet {
        entry["snippet"] = json!(snippet);
    }
    if let Some(sym) = &f.enclosing_symbol {
        entry["enclosing_symbol"] = json!(sym);
    }
    entry
}

/// Machine-readable report for CI parsing.
pub fn to_json(report: &CheckReport) -> serde_json::Value {
    json!({
        "passed": !report.failed(),
        "diff_mode": report.diff_mode,
        "severity_threshold": report.threshold.as_str(),
        "scanned_files": report.scanned_files,
        "violations": report.blocking.iter().map(finding_json).collect::<Vec<_>>(),
        "below_threshold": report.below_threshold.iter().map(finding_json).collect::<Vec<_>>(),
        "waived": report.waived.iter().map(|w| {
            let mut entry = finding_json(&w.finding);
            entry["waived"] = json!(true);
            entry["rationale"] = json!(w.rationale);
            entry["waived_by"] = json!(w.waived_by);
            entry
        }).collect::<Vec<_>>(),
        "justified": review::justified(&report.waived).map(review::justified_json).collect::<Vec<_>>(),
        "parse_errors": report.parse_errors.iter().map(|e| json!({
            "severity": "blocking",
            "index": e.index,
            "name": e.name,
            "error": e.error,
        })).collect::<Vec<_>>(),
        "sibling_patterns": report.sibling_patterns.as_ref().map(sibling_pattern::Advisory::to_json),
        "orphans": report.orphans.as_ref().map(orphans::Advisory::to_json),
        "dup_exists": report.dup_exists.as_ref().map(dup_exists::Advisory::to_json),
        "constraint_firing_log_error": report.firing_log_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constraints::finding::FindingDelta;
    use std::sync::Arc;

    fn finding(kind: &str, severity: Severity, path: &str, line: Option<u32>) -> ConstraintFinding {
        ConstraintFinding {
            constraint_id: Arc::from("abcd1234"),
            constraint_name: Some(Arc::from("test-rule")),
            constraint_kind: kind.to_string(),
            severity,
            provenance: Some(Arc::from("CLAUDE.md")),
            from_path: path.to_string(),
            to_path: String::new(),
            component_context: None,
            detail: "detail here".to_string(),
            delta: FindingDelta::Unknown,
            line,
            snippet: line.map(|_| "bad.clone()".to_string()),
            enclosing_symbol: None,
            justification: None,
            justify_marker: None,
        }
    }

    fn report(blocking: Vec<ConstraintFinding>, parse_errors: usize) -> CheckReport {
        CheckReport {
            blocking,
            below_threshold: Vec::new(),
            waived: Vec::new(),
            parse_errors: (0..parse_errors)
                .map(|i| ConstraintParseError {
                    index: i,
                    name: None,
                    error: "bad toml".to_string(),
                })
                .collect(),
            diff_mode: "staged".to_string(),
            threshold: Severity::Blocking,
            scanned_files: 3,
            sibling_patterns: None,
            orphans: None,
            dup_exists: None,
            firing_log_error: None,
        }
    }

    #[test]
    fn failed_on_any_blocking_violation() {
        let r = report(
            vec![finding(
                "forbidden_pattern",
                Severity::Blocking,
                "src/a.rs",
                Some(5),
            )],
            0,
        );
        assert!(r.failed());
        assert_eq!(to_json(&r)["passed"], serde_json::json!(false));
    }

    #[test]
    fn failed_on_parse_errors_even_with_no_violations() {
        let r = report(Vec::new(), 1);
        assert!(r.failed());
    }

    #[test]
    fn clean_report_passes() {
        let r = report(Vec::new(), 0);
        assert!(!r.failed());
        assert_eq!(to_json(&r)["passed"], serde_json::json!(true));
        assert!(render_human(&r).contains("check passed"));
    }

    #[test]
    fn human_render_locates_file_and_line() {
        let r = report(
            vec![finding(
                "forbidden_pattern",
                Severity::Blocking,
                "src/a.rs",
                Some(42),
            )],
            0,
        );
        let out = render_human(&r);
        assert!(out.contains("check failed"));
        assert!(out.contains("src/a.rs:42"));
        assert!(out.contains("[blocking]"));
        assert!(out.contains("bad.clone()"));
    }

    #[test]
    fn workspace_scoped_finding_without_line_renders_placeholder() {
        // max_fan_in / dead_constraint carry no line; a truly path-less finding
        // (config error) should still render a stable location token.
        let mut f = finding("dead_constraint", Severity::Informational, "", None);
        f.snippet = None;
        let r = report(vec![f], 0);
        let out = render_human(&r);
        assert!(out.contains("<workspace>"));
    }
}
