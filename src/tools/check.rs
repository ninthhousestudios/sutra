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
use crate::error::Result;
use crate::parser::adapter::LanguageRegistry;
use crate::rules::{ConstraintParseError, Severity};
use crate::tools::review;
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
    let (changed_paths, base_revision, _head) =
        review::resolve_diff_scope(workspace_root, diff_mode)?;
    let findings = review::build_findings(
        db,
        workspace_root,
        &changed_paths,
        &base_revision,
        None,
        registry,
    )?;

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

    out
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
        "parse_errors": report.parse_errors.iter().map(|e| json!({
            "severity": "blocking",
            "index": e.index,
            "name": e.name,
            "error": e.error,
        })).collect::<Vec<_>>(),
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
