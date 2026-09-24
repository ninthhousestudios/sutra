use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::components;
use crate::constraints::DdEngine;
use crate::constraints::check::{self, ContentSource, DiffImportEdges, EvalScope, FactsSource};
use crate::db::Db;
use crate::db::{HealthFindingRow, SnapshotCompleteness, SnapshotFileRow};
use crate::error::Result;
use crate::freshness::{self, FreshnessLevel};
use crate::git;
use crate::health::assess::PersistentEvidence;
use crate::health::compare::{
    self, BaselineSelector, IncomparableReason, MarginalEffect, SideSummary,
};
use crate::health::ondemand::OnDemandEvidence;
use crate::health::refresh::DemandOutcome;
use crate::health::scoring::{EvidencePart, ScoreValue};
use crate::parser::adapter::LanguageRegistry;
use crate::rules;
use crate::tools::change_signals::{self, ChurnMap};
use crate::tools::file_health::{missing_json, score_value_json, stored_score_json};
use crate::tools::scoring::{self, Signal};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReviewArgs {
    #[serde(default)]
    pub workspace: String,
    /// "branch" (default), "staged", "unstaged", or a commit spec (e.g. "HEAD~3..HEAD", "abc123")
    #[serde(default)]
    pub diff: Option<String>,
    /// When true, include `_explain` with weights, ceilings, and per-signal contributions.
    #[serde(default)]
    pub explain: Option<bool>,
}

const MAX_AFFECTED: usize = 20;
const MAX_READS: usize = 10;

// Renormalized after removing the deviations factor (sutra/313): weights
// previously summed to 1.0 including deviations (weight 0.2); scaling the
// remaining four by 1/0.8 preserves that invariant.
const W_BLAST: f64 = 0.375;
const W_COMPLEXITY: f64 = 0.25;
const W_HOTSPOT: f64 = 0.1875;
const W_CHURN: f64 = 0.1875;

pub use crate::constraints::ConstraintFinding;
use crate::waivers::Waived;

#[derive(Default)]
pub struct ReviewFindings {
    pub constraint_violations: Vec<ConstraintFinding>,
    pub resolved_constraint_violations: Vec<ConstraintFinding>,
    pub waived_constraint_violations: Vec<Waived<ConstraintFinding>>,
    pub constraint_parse_errors: Vec<rules::ConstraintParseError>,
    pub constraint_violations_total: usize,
    /// Report-only instance acks (sutra/305) on changed files, as JSON. Surfaced
    /// so acknowledged clones dropped from `constraint_violations` stay visible on
    /// the review surface, not silent (sutra/306) — parity with waivers.
    pub acknowledged: Vec<serde_json::Value>,
    /// Operator-facing warnings from resolving `.sutra/accepted.toml` against the
    /// live rules (unknown/ambiguous constraint refs). Surfaced so a waiver
    /// pointing at a deleted constraint is visible, not silently inert
    /// (sutra/308 hazard 4).
    pub accepted_warnings: Vec<String>,
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    diff_mode: Option<&str>,
    dd_engine: Option<&DdEngine>,
    // Baseline checkpoint pinned by the caller BEFORE `tool_context` could
    // full-parse and record a newer snapshot (sutra/415). `Pinned(None)` is a
    // genuinely-missing baseline → incomparable (sutra/424 F5).
    baseline: BaselineSelector,
    // Outcome of the persistent health refresh performed under the caller's DD
    // lock. Only a reuse/publication certifies the current run; otherwise its
    // outcomes read as Missing, so the temporal side is partial (never a
    // measured change) and attribution is only conditional (sutra/416).
    health_refresh: DemandOutcome,
    explain: bool,
) -> Result<serde_json::Value> {
    let mode = diff_mode.unwrap_or("branch");

    let scope = resolve_diff_entries(workspace_root, mode)?;
    let changed_paths = scope.paths();
    let (base_revision, head_revision) = (&scope.base_revision, &scope.head_revision);

    let churn = ChurnMap {
        counts: git::git_churn(workspace_root, change_signals::CHURN_WINDOW_DAYS)?,
        window_days: change_signals::CHURN_WINDOW_DAYS,
    };

    let registry = crate::parser::adapter::default_registry();
    let (findings, findings_error) = match build_findings(
        db,
        workspace_root,
        &changed_paths,
        base_revision,
        // The compositor assesses current state — read the working tree, not the
        // diff snapshot, regardless of `diff_mode` (sutra/385).
        ContentSource::Worktree,
        dd_engine,
        &registry,
    ) {
        Ok(f) => (f, None),
        Err(e) => (ReviewFindings::default(), Some(e.to_string())),
    };

    let shape_config = crate::similarity::diff::ShapeChangeConfig::default();
    let shape_diff = crate::similarity::diff::detect_shape_changes(
        db,
        workspace_root,
        &changed_paths,
        base_revision,
        head_revision.as_deref(),
        &registry,
        &shape_config,
    );

    // Health: temporal comparison of persistent evidence (baseline checkpoint vs
    // the current run, validity from the refresh under our lock) and, separately,
    // attribution of fresh on-demand evidence against that same current run
    // (health-evidence-contract.md § Comparison and scoring). A failure is
    // surfaced as `health_delta_error`, never swallowed into "no change".
    let health = review_health(
        db,
        workspace_root,
        &changed_paths,
        &shape_diff,
        shape_config.hrr_delta_threshold,
        baseline,
        health_refresh,
    );
    let shape_changes = shape_diff.changes;

    let erosion_delta = crate::tools::erosion_delta::compute(
        workspace_root,
        &scope.entries,
        base_revision,
        head_revision.as_deref(),
        &registry,
    );

    let mut result = compute(
        db,
        workspace_root,
        &changed_paths,
        &churn,
        &findings,
        explain,
    )?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("diff_mode".into(), json!(mode));
        obj.insert(
            "churn_window_days".into(),
            json!(change_signals::CHURN_WINDOW_DAYS),
        );
        if let Some(err) = findings_error {
            obj.insert("findings_degraded".into(), json!(true));
            obj.insert("findings_error".into(), json!(err));
            obj.insert("risk_score".into(), json!(null));
        }

        let shape_out: Vec<_> = shape_changes
            .iter()
            .filter(|c| c.quadrant == crate::similarity::diff::DiffQuadrant::SubtleStructural)
            .map(|c| {
                json!({
                    "file": c.file_path,
                    "symbol": c.symbol_name,
                    "text_delta": scoring::round3(c.text_delta),
                    "hrr_delta": scoring::round3(c.hrr_delta),
                    "quadrant": c.quadrant.as_str(),
                    "detail": format!(
                        "{}: text changed {:.0}% but structural shape changed {:.0}%",
                        c.symbol_name, c.text_delta * 100.0, c.hrr_delta * 100.0,
                    ),
                })
            })
            .collect();
        if !shape_out.is_empty() {
            obj.insert("hrr_shape_changes".into(), json!(shape_out));
        }

        if let Some(block) = crate::tools::erosion_delta::delta_json(&erosion_delta) {
            obj.insert("erosion_delta".into(), block);
        }

        match health {
            Ok(h) => {
                if !h.findings.is_empty() {
                    obj.insert("health_findings".into(), json!(h.findings));
                }
                obj.insert("health_delta".into(), h.delta);
            }
            Err(e) => {
                obj.insert("health_delta_error".into(), json!(e.to_string()));
            }
        }
    }
    Ok(result)
}

struct ReviewHealth {
    /// On-demand findings (display list), each flagged `waived`.
    findings: Vec<serde_json::Value>,
    delta: serde_json::Value,
}

fn review_health(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    shape_diff: &crate::similarity::diff::ShapeDiff<'_>,
    hrr_threshold: f64,
    baseline: BaselineSelector,
    health_refresh: DemandOutcome,
) -> Result<ReviewHealth> {
    let mut ondemand =
        crate::health::ondemand::compute_ondemand_findings(db, workspace_root, changed_paths)?;
    ondemand.add_shape_diff(shape_diff, hrr_threshold);
    let evidence = PersistentEvidence::load(db, health_refresh.verdict())?;
    let baseline_id = baseline.resolve(db)?;
    let baseline_rows = match baseline_id {
        Some(id) => db.snapshot_file_scores(id)?,
        None => Vec::new(),
    };
    let OnDemandEvidence { findings, outcomes } = ondemand;
    let (active, waived) = partition_ondemand(db, findings, &evidence)?;

    let findings_out: Vec<serde_json::Value> = active
        .iter()
        .map(|f| (f, false))
        .chain(waived.iter().map(|f| (f, true)))
        .map(|(f, is_waived)| {
            json!({
                "biomarker": f.biomarker_kind,
                "severity": f.severity,
                "file_id": f.file_id,
                "symbol_id": f.symbol_id,
                "metric_value": scoring::round3(f.metric_value),
                "threshold": scoring::round3(f.threshold),
                "detail": f.detail,
                "waived": is_waived,
            })
        })
        .collect();

    let mut active_by_file: HashMap<i64, Vec<HealthFindingRow>> = HashMap::new();
    for f in active {
        active_by_file.entry(f.file_id).or_default().push(f);
    }

    let base_by_path: HashMap<&str, &SnapshotFileRow> = baseline_rows
        .iter()
        .map(|r| (r.file_path.as_str(), r))
        .collect();
    let mut files = Vec::new();
    for path in changed_paths {
        let current = evidence.file(path);
        let base = base_by_path.get(path.as_str()).copied();
        if current.is_none() && base.is_none() {
            continue;
        }
        let score = current.map(|e| e.score());
        let basis = current.map(|e| e.basis.to_hex());
        let current_side = score
            .as_ref()
            .zip(basis.as_deref())
            .map(|(s, b)| SideSummary::of_score(s, b));
        let base_side = base.map(|r| SideSummary {
            completeness: r.completeness,
            basis: r.score_basis.as_deref(),
        });
        let blocker = if baseline_id.is_none() {
            Some(IncomparableReason::MissingBaseline)
        } else {
            compare::temporal_blocker(base_side, current_side)
        };

        let current_json = score.as_ref().map(|s| {
            let mut m = score_value_json(&s.value);
            m.insert(
                "completeness".into(),
                json!(if s.value.is_measured() {
                    "complete"
                } else {
                    "partial"
                }),
            );
            if !s.missing.is_empty() {
                m.insert("missing_biomarkers".into(), json!(s.missing_names()));
            }
            serde_json::Value::Object(m)
        });
        let base_json = base.map(baseline_observation_json);
        let (temporal, temporal_noteworthy) = match (blocker, &score, base) {
            (None, Some(s), Some(b)) => {
                let delta = s.value.upper() - b.score;
                (
                    json!({
                        "measured": true,
                        "delta": scoring::round3(delta),
                        "from": base_json,
                        "to": current_json,
                    }),
                    delta.abs() >= 0.005,
                )
            }
            (reason, _, _) => {
                let reason = reason.unwrap_or(IncomparableReason::NewFile);
                let changed = match (&score, base) {
                    (Some(s), Some(b)) => observation_changed(&s.value, b, basis.as_deref()),
                    _ => true,
                };
                let noteworthy = reason != IncomparableReason::MissingBaseline && changed;
                (
                    json!({
                        "measured": false,
                        "reason": reason.as_str(),
                        "from": base_json,
                        "to": current_json,
                    }),
                    noteworthy,
                )
            }
        };

        let (on_demand, attribution_noteworthy) = match current {
            Some(e) => {
                let rows = active_by_file.remove(&e.file_id).unwrap_or_default();
                let file_outcomes = outcomes.get(path.as_str()).map_or(&[][..], Vec::as_slice);
                if file_outcomes.is_empty() && rows.is_empty() {
                    (serde_json::Value::Null, false)
                } else {
                    attribution_json(e.part(), file_outcomes, &rows)
                }
            }
            None => (serde_json::Value::Null, false),
        };

        if temporal_noteworthy || attribution_noteworthy {
            let mut entry = json!({ "path": path, "temporal": temporal });
            if !on_demand.is_null() {
                entry["on_demand"] = on_demand;
            }
            files.push(entry);
        }
    }

    let baseline_run = match baseline_id {
        Some(id) => db.snapshot_health_run_id(id)?,
        None => None,
    };
    let current_run = evidence.run_id.map(|r| r.0);
    Ok(ReviewHealth {
        findings: findings_out,
        delta: json!({
            "baseline_snapshot_id": baseline_id,
            "baseline_run_id": baseline_run,
            "current_run_id": current_run,
            // Input axes that moved between the baseline's run and the current
            // run; a measured delta is not by itself caused by the diff.
            "input_changes": compare::input_changes_json(db, baseline_run, current_run)?,
            "persistent_validity": health_refresh.validity(),
            "temporal_incomparable": baseline_id.is_none().then_some("missing_baseline"),
            "files": files,
        }),
    })
}

/// Waiver-partition the on-demand findings under the same policy as persistent
/// findings (path + symbol label), returning `(active, waived)` rows.
fn partition_ondemand(
    db: &Db,
    findings: Vec<crate::health::HealthFinding>,
    evidence: &PersistentEvidence,
) -> Result<(Vec<HealthFindingRow>, Vec<HealthFindingRow>)> {
    use crate::waivers::{self, ResolvedHealthFinding};
    let path_of: HashMap<i64, &str> = evidence
        .files
        .iter()
        .map(|f| (f.file_id, f.path.as_str()))
        .collect();
    let rows: Vec<HealthFindingRow> = findings
        .into_iter()
        .enumerate()
        .map(|(i, f)| f.into_row(-(i as i64) - 1))
        .collect();
    let resolved: Vec<ResolvedHealthFinding> =
        crate::health::assess::label_findings(db, rows, &path_of)?
            .into_iter()
            .map(ResolvedHealthFinding::from)
            .collect();
    let (active, waived) = waivers::partition(resolved, evidence.waivers());
    Ok((
        active.into_iter().map(|r| r.finding).collect(),
        waived.into_iter().map(|w| w.finding.finding).collect(),
    ))
}

/// Whether a current observation differs from its baseline row in score,
/// completeness or basis — an incomparable pair is still worth reporting then.
fn observation_changed(current: &ScoreValue, base: &SnapshotFileRow, basis: Option<&str>) -> bool {
    let completeness = if current.is_measured() {
        SnapshotCompleteness::Complete
    } else {
        SnapshotCompleteness::Partial
    };
    (current.lower() - base.score).abs() >= 0.005
        || completeness != base.completeness
        || basis != base.score_basis.as_deref()
}

/// A baseline checkpoint observation, serialized through the shared stored-score
/// shape (legacy rows surface as `legacy_score`, never as a bound).
fn baseline_observation_json(r: &SnapshotFileRow) -> serde_json::Value {
    let mut m = stored_score_json(
        r.completeness,
        r.score_basis.is_some(),
        r.score,
        r.score_upper,
    );
    m.insert("completeness".into(), json!(r.completeness.as_str()));
    if !r.missing_biomarkers.is_empty() {
        m.insert("missing_biomarkers".into(), json!(r.missing_biomarkers));
    }
    serde_json::Value::Object(m)
}

/// On-demand attribution for one file, and whether it is worth reporting.
fn attribution_json(
    persistent: EvidencePart<'_>,
    outcomes: &[crate::health::scoring::ProducerResult],
    rows: &[HealthFindingRow],
) -> (serde_json::Value, bool) {
    let a = compare::attribute(
        persistent,
        EvidencePart {
            outcomes,
            findings: rows,
        },
    );
    let effect = match a.effect {
        MarginalEffect::Exact(v) => json!({ "kind": "exact", "value": scoring::round3(v) }),
        MarginalEffect::Conditional { lower, upper } => json!({
            "kind": "conditional",
            "lower": scoring::round3(lower),
            "upper": scoring::round3(upper),
        }),
    };
    let findings: Vec<_> = a
        .findings
        .iter()
        .map(|f| {
            let row = &rows[f.index];
            json!({
                "biomarker": row.biomarker_kind,
                "detail": row.detail,
                "raw_deduction": scoring::round3(f.raw_deduction),
                "scaled_deduction": scoring::round3(f.scaled_deduction),
                "marginal": scoring::round3(f.marginal),
            })
        })
        .collect();
    let noteworthy = !findings.is_empty() || !a.missing.is_empty();
    let mut out = json!({
        "without": serde_json::Value::Object(score_value_json(&a.without)),
        "with": serde_json::Value::Object(score_value_json(&a.with)),
        "effect": effect,
        "findings": findings,
    });
    if !a.missing.is_empty() {
        out["missing"] = missing_json(&a.missing);
    }
    (out, noteworthy)
}

/// A resolved diff: the changed files (renames carry `old_path`) and the two
/// revisions they are compared between.
pub struct DiffScope {
    pub entries: Vec<git::DiffFileEntry>,
    /// `""` (the index) for unstaged, `HEAD` for staged, an explicit revision
    /// otherwise. Read with `git show {base}:{path}`, so `""` resolves to the index.
    pub base_revision: String,
    /// `Some("")` for staged (the index), `None` for unstaged (the worktree), an
    /// explicit revision otherwise.
    pub head_revision: Option<String>,
}

impl DiffScope {
    pub fn paths(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.path.to_string()).collect()
    }
}

/// Resolve a diff-mode string to `(changed_paths, base_revision, head_revision)`.
///
/// Shared by the review compositor and the `sutra check` CLI gate so both
/// interpret `"staged"` / `"unstaged"` / `"branch"` / a commit spec identically.
/// `head_revision` is `Some("")` for staged (the index), `None` for unstaged
/// (the worktree), and an explicit revision otherwise; `base_revision` is `""`
/// (the index) for unstaged, so both sides match what `git diff` compared.
pub fn resolve_diff_scope(
    workspace_root: &Path,
    mode: &str,
) -> Result<(Vec<String>, String, Option<String>)> {
    let scope = resolve_diff_entries(workspace_root, mode)?;
    let paths = scope.paths();
    Ok((paths, scope.base_revision, scope.head_revision))
}

/// [`resolve_diff_scope`] keeping the per-file entries, so a rename's base side
/// is read from its old path. Every mode detects renames (`--name-status -M`).
pub fn resolve_diff_entries(workspace_root: &Path, mode: &str) -> Result<DiffScope> {
    let (entries, base_revision, head_revision) = match mode {
        "staged" => (
            git::git_diff_staged_entries(workspace_root)?,
            "HEAD".to_string(),
            Some(String::new()),
        ),
        // `git diff` compares the index to the worktree, so the base side is
        // the index too: reading HEAD would leak staged changes into an
        // unstaged review (sutra/458).
        "unstaged" => (
            git::git_diff_unstaged_entries(workspace_root)?,
            String::new(),
            None,
        ),
        "branch" => {
            let default_branch = git::detect_default_branch(workspace_root)?;
            let base = git::git_merge_base(workspace_root, &default_branch)?;
            let entries = git::git_diff_files(workspace_root, &base, "HEAD")?;
            (entries, base, Some("HEAD".to_string()))
        }
        spec => {
            let (base, head) = if let Some((a, b)) = spec.split_once("..") {
                (a.to_string(), b.to_string())
            } else {
                (format!("{spec}~1"), spec.to_string())
            };
            let entries = git::git_diff_files(workspace_root, &base, &head)?;
            (entries, base, Some(head))
        }
    };
    Ok(DiffScope {
        entries,
        base_revision,
        head_revision,
    })
}

fn extract_outgoing_edges(
    content: &str,
    rel_path: &str,
    file_id: i64,
    workspace_root: &Path,
    id_map: &HashMap<&str, i64>,
) -> Option<Vec<(i64, i64)>> {
    let language = if rel_path.ends_with(".rs") {
        "rust"
    } else if rel_path.ends_with(".dart") {
        "dart"
    } else {
        return None;
    };
    let result = match crate::parser::parse_file(content, language, rel_path) {
        Ok(r) if r.parsed_ok => r,
        _ => return None,
    };
    let mut edges = Vec::new();
    match language {
        "rust" => {
            let layout = crate::rust_imports::parse_workspace_layout(workspace_root);
            let path_ref_map: HashMap<&str, i64> = id_map.iter().map(|(k, v)| (*k, *v)).collect();
            for import in &result.imports {
                let resolved = match crate::rust_imports::normalize_to_crate_segments(
                    &import.raw_path,
                    rel_path,
                    &layout,
                ) {
                    Some(r) if !r.segments.is_empty() => r,
                    _ => continue,
                };
                if let Some(target_id) = crate::rust_imports::resolve_segments(
                    &resolved.segments,
                    &path_ref_map,
                    &resolved.src_prefix,
                ) && target_id != file_id
                {
                    edges.push((file_id, target_id));
                }
            }
        }
        "dart" => {
            let pkg_map = crate::dart_packages::DartPackageMap::build(workspace_root);
            let id_to_path: HashMap<i64, &str> = id_map.iter().map(|(k, v)| (*v, *k)).collect();
            for import in &result.imports {
                let resolved = if import.raw_path.starts_with("package:") {
                    crate::dart_packages::resolve_package_uri(&import.raw_path, &pkg_map)
                } else if import.raw_path.ends_with(".dart")
                    && !import.raw_path.starts_with("dart:")
                {
                    crate::dart_packages::resolve_relative_import(
                        &import.raw_path,
                        file_id,
                        &id_to_path,
                    )
                } else {
                    None
                };
                if let Some(path) = resolved
                    && let Some(&target_id) = id_map.get(path.as_str())
                    && target_id != file_id
                {
                    edges.push((file_id, target_id));
                }
            }
        }
        _ => {}
    }
    Some(edges)
}

pub fn build_findings(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    base_revision: &str,
    // Where forbidden-pattern content is read for the scoped files. `sutra check`
    // passes the requested snapshot (the staged index or a commit); the review
    // compositor passes `Worktree` to preserve its assess-current-state contract.
    content: ContentSource,
    shared_dd: Option<&DdEngine>,
    registry: &LanguageRegistry,
) -> Result<ReviewFindings> {
    let _rules = rules::load_rules(workspace_root)?;
    let all_files = db.all_files()?;
    let id_map: HashMap<&str, i64> = all_files.iter().map(|f| (&*f.path, f.id)).collect();

    // Constraint evaluation via unified check core
    let changed_ids: HashSet<i64> = changed_paths
        .iter()
        .filter_map(|p| id_map.get(p.as_str()).copied())
        .collect();

    let mut old_edges: HashSet<(i64, i64)> = HashSet::new();
    let mut import_delta = DiffImportEdges::default();
    for path in changed_paths {
        let file_id = match id_map.get(path.as_str()) {
            Some(&id) => id,
            None => continue,
        };
        let old_content = match git::git_file_content_at(workspace_root, base_revision, path) {
            Ok(Some(c)) => c,
            Ok(None) => {
                import_delta.added_ids.insert(file_id);
                continue;
            }
            Err(_) => continue,
        };
        let Some(base_edges) =
            extract_outgoing_edges(&old_content, path, file_id, workspace_root, &id_map)
        else {
            continue;
        };
        old_edges.extend(base_edges.iter().copied());
        // Fan-in attribution compares like with like: the same extractor over
        // the requested snapshot, and only when both sides extracted (sutra/440).
        if let Some(head_content) = check::read_scoped_content(workspace_root, content, path)
            && let Some(head_edges) =
                extract_outgoing_edges(&head_content, path, file_id, workspace_root, &id_map)
        {
            import_delta.base_edges.extend(base_edges);
            import_delta.head_edges.extend(head_edges);
        }
    }

    // Changed stubs have no file row, so they never make it into changed_ids.
    let changed_pattern_only_paths: Vec<String> = changed_paths
        .iter()
        .filter(|p| crate::constraints::patterns::is_pattern_only_path(p, registry))
        .cloned()
        .collect();

    let changed_set: HashSet<&str> = changed_paths.iter().map(|p| p.as_str()).collect();

    let check_outcome = check::evaluate(
        &FactsSource::DdBacked {
            db,
            dd_engine: shared_dd,
        },
        workspace_root,
        EvalScope::ChangedFiles {
            changed_ids: &changed_ids,
            old_edges: &old_edges,
            import_delta: &import_delta,
            changed_pattern_only_paths: &changed_pattern_only_paths,
            content,
            changed_paths: &changed_set,
        },
        registry,
    )?;

    let constraint_violations = check_outcome.active;
    let resolved_constraint_violations = check_outcome.resolved;
    let waived_constraint_violations = check_outcome.waived;
    let constraint_violations_total =
        constraint_violations.len() + waived_constraint_violations.len();
    let constraint_parse_errors = check_outcome.parse_errors;
    let accepted_warnings = check_outcome.accepted_warnings;

    // Report-only instance acks on the changed files, so acknowledged clones
    // dropped from constraint_violations stay visible here (sutra/306).
    let acknowledged = crate::tools::constraints::acked_instances_json(db, Some(&changed_set))?;

    Ok(ReviewFindings {
        constraint_violations,
        resolved_constraint_violations,
        waived_constraint_violations,
        constraint_parse_errors,
        constraint_violations_total,
        acknowledged,
        accepted_warnings,
    })
}

fn file_freshness(db: &Db, workspace_root: &Path, path: &str) -> FreshnessLevel {
    db.file_by_path(path)
        .ok()
        .flatten()
        .map(|f| freshness::check_file(workspace_root, path, &f.last_parsed).into())
        .unwrap_or(FreshnessLevel::StaleIndex)
}

fn behavioral_coupling(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
) -> Vec<serde_json::Value> {
    let config = match components::load_config(workspace_root) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let threshold = config.cochange_threshold.unwrap_or(0.5);

    let mut changed_ids: HashMap<i64, &str> = HashMap::new();
    for p in changed_paths {
        if let Ok(Some(f)) = db.file_by_path(p) {
            changed_ids.insert(f.id, p.as_str());
        }
    }
    if changed_ids.is_empty() {
        return Vec::new();
    }

    let cochange_pairs = match db.cochange_pairs_above_threshold(threshold) {
        Ok(pairs) => pairs,
        Err(_) => return Vec::new(),
    };

    let all_files: HashMap<i64, Arc<str>> = db
        .all_files()
        .unwrap_or_default()
        .into_iter()
        .map(|f| (f.id, f.path))
        .collect();

    let static_edges: HashSet<(i64, i64)> = db
        .static_file_edges()
        .unwrap_or_default()
        .into_iter()
        .collect();

    let mut entries: Vec<(f64, serde_json::Value)> = cochange_pairs
        .into_iter()
        .filter_map(|(fa, fb, jaccard, shared)| {
            let (changed_id, partner_id) =
                if changed_ids.contains_key(&fa) && !changed_ids.contains_key(&fb) {
                    (fa, fb)
                } else if changed_ids.contains_key(&fb) && !changed_ids.contains_key(&fa) {
                    (fb, fa)
                } else {
                    return None;
                };
            if static_edges.contains(&(changed_id.min(partner_id), changed_id.max(partner_id))) {
                return None;
            }
            let changed_path = changed_ids.get(&changed_id)?;
            let partner_path = all_files.get(&partner_id)?;
            if components::is_test_file(changed_path) != components::is_test_file(partner_path) {
                return None;
            }
            Some((
                jaccard,
                json!({
                    "changed_file": changed_path,
                    "partner": partner_path,
                    "jaccard": jaccard,
                    "shared_commits": shared,
                }),
            ))
        })
        .collect();

    entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    entries.into_iter().map(|(_, v)| v).collect()
}

fn build_recommended_reads(
    db: &Db,
    workspace_root: &Path,
    affected_files: &[change_signals::AffectedFile],
    behavioral_partners: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut seen = std::collections::HashSet::new();
    let mut reads: Vec<(String, i64, bool)> = Vec::new();
    for bp in behavioral_partners {
        if let Some(partner) = bp["partner"].as_str()
            && seen.insert(partner.to_string())
        {
            let blast = db
                .file_by_path(partner)
                .ok()
                .flatten()
                .map(|f| f.blast_radius)
                .unwrap_or(0);
            reads.push((partner.to_string(), blast, true));
        }
    }
    for a in affected_files {
        if !seen.contains(&a.path) {
            reads.push((a.path.clone(), a.blast_radius, false));
        }
    }
    reads.truncate(MAX_READS);

    reads
        .iter()
        .map(|(path, blast, is_behavioral)| {
            let fl = file_freshness(db, workspace_root, path);
            let mut entry = json!({ "path": path, "blast_radius": blast, "_freshness": fl });
            if *is_behavioral {
                entry["behavioral_partner"] = json!(true);
            }
            entry
        })
        .collect()
}

pub fn compute(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    churn: &ChurnMap,
    findings: &ReviewFindings,
    explain: bool,
) -> Result<serde_json::Value> {
    if changed_paths.is_empty() {
        let mut result = json!({
            "changed_files": [],
            "changed_symbols": [],
            "affected_files": [],
            "affected_symbols": [],
            "affected_total": { "files": 0, "symbols": 0 },
            "risk_score": 0.0,
            "risk_breakdown": {
                "blast_radius": 0.0, "complexity_delta": 0.0,
                "hotspot_overlap": 0.0, "churn": 0.0,
            },
            "recommended_reads": [],
            "constraint_violations": [],
            "resolved_constraint_violations": [],
            "constraint_violations_total": 0,
            "waived_constraint_violations": [],
        });
        if explain {
            result["_explain"] = json!({
                "formula": "sum(weight_i * min(raw_i / ceiling_i, 1.0)), clamped to [0, 1]",
                "weights": {
                    "blast_radius": { "weight": W_BLAST, "ceiling": change_signals::BLAST_NORM, "contribution": 0.0, "rationale": "blast radius of changed symbols" },
                    "complexity": { "weight": W_COMPLEXITY, "ceiling": change_signals::COMPLEXITY_NORM, "contribution": 0.0, "rationale": "peak cognitive complexity in changed code" },
                    "hotspot_overlap": { "weight": W_HOTSPOT, "ceiling": 1.0, "contribution": 0.0, "rationale": "proportion of changed files that are churn hotspots" },
                    "churn": { "weight": W_CHURN, "ceiling": change_signals::CHURN_NORM, "contribution": 0.0, "rationale": "total recent churn across changed files" },
                },
            });
        }
        return Ok(result);
    }

    let signals = change_signals::gather(db, changed_paths, churn, true)?;

    let changed_files_out: Vec<_> = signals
        .per_file
        .iter()
        .map(|f| {
            let fl = file_freshness(db, workspace_root, &f.path);
            json!({
                "path": f.path, "blast_radius": f.blast_radius,
                "symbol_count": f.symbols.len(), "_freshness": fl,
            })
        })
        .collect();
    let changed_symbols_out: Vec<_> = signals
        .per_file
        .iter()
        .flat_map(|f| {
            f.symbols.iter().map(|s| {
                json!({
                    "symbol": s.qualified_name, "file": f.path, "cognitive": s.cognitive,
                })
            })
        })
        .collect();

    let total_affected_files = signals.affected_files.len();
    let total_affected_symbols = signals.affected_symbols.len();

    let affected_files_out: Vec<_> = signals
        .affected_files
        .iter()
        .take(MAX_AFFECTED)
        .map(|a| {
            let fl = file_freshness(db, workspace_root, &a.path);
            json!({ "path": a.path, "blast_radius": a.blast_radius, "_freshness": fl })
        })
        .collect();
    let affected_symbols_out: Vec<_> = signals
        .affected_symbols
        .iter()
        .take(MAX_AFFECTED)
        .map(|a| {
            let fl = file_freshness(db, workspace_root, &a.file);
            json!({ "symbol": a.qualified_name, "file": a.file, "blast_radius": a.blast_radius, "cognitive": a.cognitive, "_freshness": fl })
        })
        .collect();

    let constraint_violations_out: Vec<_> = findings
        .constraint_violations
        .iter()
        .map(|v| {
            let mut entry = json!({
                "constraint_id": v.constraint_id,
                "constraint_name": v.constraint_name,
                "kind": v.constraint_kind,
                "severity": v.severity.as_str(),
                "provenance": v.provenance,
                "from": v.from_path,
                "to": v.to_path,
                "component_context": v.component_context,
                "detail": v.detail,
            });
            if let Some(line) = v.line {
                entry["line"] = json!(line);
            }
            if let Some(snippet) = &v.snippet {
                entry["snippet"] = json!(snippet);
            }
            if let Some(sym) = &v.enclosing_symbol {
                entry["enclosing_symbol"] = json!(sym);
            }
            entry
        })
        .collect();
    let resolved_constraint_violations_out: Vec<_> = findings
        .resolved_constraint_violations
        .iter()
        .map(|v| {
            let mut entry = json!({
                "constraint_id": v.constraint_id,
                "constraint_name": v.constraint_name,
                "kind": v.constraint_kind,
                "severity": v.severity.as_str(),
                "provenance": v.provenance,
                "from": v.from_path,
                "to": v.to_path,
                "component_context": v.component_context,
                "detail": v.detail,
            });
            if let Some(line) = v.line {
                entry["line"] = json!(line);
            }
            if let Some(snippet) = &v.snippet {
                entry["snippet"] = json!(snippet);
            }
            if let Some(sym) = &v.enclosing_symbol {
                entry["enclosing_symbol"] = json!(sym);
            }
            entry
        })
        .collect();
    let waived_constraint_violations_out: Vec<_> = findings
        .waived_constraint_violations
        .iter()
        .map(|v| {
            let mut entry = json!({
                "constraint_id": v.finding.constraint_id,
                "constraint_name": v.finding.constraint_name,
                "kind": v.finding.constraint_kind,
                "severity": v.finding.severity.as_str(),
                "from": v.finding.from_path,
                "to": v.finding.to_path,
                "component_context": v.finding.component_context,
                "detail": v.finding.detail,
                "waived": true,
                "rationale": v.rationale,
                "waived_by": v.waived_by,
            });
            if let Some(line) = v.finding.line {
                entry["line"] = json!(line);
            }
            if let Some(snippet) = &v.finding.snippet {
                entry["snippet"] = json!(snippet);
            }
            if let Some(sym) = &v.finding.enclosing_symbol {
                entry["enclosing_symbol"] = json!(sym);
            }
            entry
        })
        .collect();
    let file_count = changed_paths.len();
    let blast_score = scoring::normalize(signals.total_blast as f64, change_signals::BLAST_NORM);
    let complexity_score = scoring::normalize(
        signals.max_cognitive.unwrap_or(0) as f64,
        change_signals::COMPLEXITY_NORM,
    );
    let hotspot_ceiling = (file_count as f64).max(1.0);
    let hotspot_score = scoring::normalize(signals.hotspot_files as f64, hotspot_ceiling);
    let churn_score = scoring::normalize(signals.total_churn as f64, change_signals::CHURN_NORM);

    let risk_score = scoring::weighted_score(&[
        Signal {
            weight: W_BLAST,
            score: blast_score,
        },
        Signal {
            weight: W_COMPLEXITY,
            score: complexity_score,
        },
        Signal {
            weight: W_HOTSPOT,
            score: hotspot_score,
        },
        Signal {
            weight: W_CHURN,
            score: churn_score,
        },
    ]);

    let behavioral = behavioral_coupling(db, workspace_root, changed_paths);
    let recommended_reads =
        build_recommended_reads(db, workspace_root, &signals.affected_files, &behavioral);

    let mut result = json!({
        "changed_files": changed_files_out,
        "changed_symbols": changed_symbols_out,
        "affected_files": affected_files_out,
        "affected_symbols": affected_symbols_out,
        "affected_total": {
            "files": total_affected_files,
            "files_truncated": total_affected_files > MAX_AFFECTED,
            "symbols": total_affected_symbols,
            "symbols_truncated": total_affected_symbols > MAX_AFFECTED,
        },
        "risk_score": scoring::round3(risk_score),
        "risk_breakdown": {
            "blast_radius": scoring::round3(blast_score),
            "complexity_delta": scoring::round3(complexity_score),
            "hotspot_overlap": scoring::round3(hotspot_score),
            "churn": scoring::round3(churn_score),
        },
        "constraint_violations": constraint_violations_out,
        "resolved_constraint_violations": resolved_constraint_violations_out,
        "constraint_violations_total": findings.constraint_violations_total,
        "waived_constraint_violations": waived_constraint_violations_out,
        "recommended_reads": recommended_reads,
    });
    if !behavioral.is_empty() {
        result["behavioral_coupling"] = json!(behavioral);
    }
    if !findings.acknowledged.is_empty() {
        result["acknowledged"] = json!(findings.acknowledged);
    }
    if !findings.accepted_warnings.is_empty() {
        result["accepted_warnings"] = json!(findings.accepted_warnings);
    }
    if !findings.constraint_parse_errors.is_empty() {
        result["constraint_parse_errors"] = json!(
            findings
                .constraint_parse_errors
                .iter()
                .map(|e| json!({
                    "severity": "blocking",
                    "index": e.index,
                    "name": e.name,
                    "error": e.error,
                    "detail": format!(
                        "malformed [[constraint]] at index {}{}: {}",
                        e.index,
                        e.name.as_deref().map(|n| format!(" (name: {n})")).unwrap_or_default(),
                        e.error,
                    ),
                }))
                .collect::<Vec<_>>()
        );
    }
    if explain {
        result["_explain"] = json!({
            "formula": "sum(weight_i * min(raw_i / ceiling_i, 1.0)), clamped to [0, 1]",
            "weights": {
                "blast_radius": { "weight": W_BLAST, "ceiling": change_signals::BLAST_NORM, "contribution": scoring::round3(W_BLAST * blast_score), "rationale": "blast radius of changed symbols" },
                "complexity": { "weight": W_COMPLEXITY, "ceiling": change_signals::COMPLEXITY_NORM, "contribution": scoring::round3(W_COMPLEXITY * complexity_score), "rationale": "peak cognitive complexity in changed code" },
                "hotspot_overlap": { "weight": W_HOTSPOT, "ceiling": hotspot_ceiling, "contribution": scoring::round3(W_HOTSPOT * hotspot_score), "rationale": "proportion of changed files that are churn hotspots" },
                "churn": { "weight": W_CHURN, "ceiling": change_signals::CHURN_NORM, "contribution": scoring::round3(W_CHURN * churn_score), "rationale": "total recent churn across changed files" },
            },
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::InsertSymbolParams;
    use crate::health::assess::RunVerdict;
    use crate::health::evidence::MissingReason;
    use crate::health::findings::{BiomarkerKind, HealthFinding, HealthSeverity};

    fn finding(file_id: i64, symbol_id: i64, kind: BiomarkerKind) -> HealthFinding {
        HealthFinding {
            file_id,
            symbol_id: Some(symbol_id),
            biomarker_kind: kind,
            severity: HealthSeverity::Advisory,
            confidence: 1.0,
            provenance: "test".to_string(),
            metric_value: 10.0,
            threshold: 5.0,
            detail: String::new(),
        }
    }

    /// sutra/437: every finding on a symbol carries its label, not just the
    /// first — a symbol-scoped waiver on a later finding must still match.
    #[test]
    fn symbol_waiver_matches_second_ondemand_finding_on_same_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_unchecked("test", dir.path()).unwrap();
        let fid = db
            .upsert_file("src/hot.rs", "rust", "abc123", 100, true)
            .unwrap();
        let sid = db
            .insert_symbol(&InsertSymbolParams {
                file_id: fid,
                qualified_name: "hot::churny",
                short_name: "churny",
                kind: "function",
                signature: None,
                signature_hash: None,
                structural_hash: None,
                visibility: Some("pub"),
                start_line: 1,
                start_col: 0,
                end_line: 10,
                end_col: 0,
                parent_symbol_id: None,
                docstring: None,
                cyclomatic: Some(1),
                cognitive: Some(0),
                max_nesting: Some(1),
                flags: 0,
                language_attrs: None,
            })
            .unwrap();
        db.create_health_waiver(
            "code_age_volatility",
            "src/hot.rs",
            Some("hot::churny"),
            "known churn",
            "josh",
        )
        .unwrap();
        let evidence =
            PersistentEvidence::load(&db, RunVerdict::stale(MissingReason::LegacyUnknown)).unwrap();

        let findings = vec![
            finding(fid, sid, BiomarkerKind::FunctionHotspot),
            finding(fid, sid, BiomarkerKind::CodeAgeVolatility),
        ];
        let (active, waived) = partition_ondemand(&db, findings, &evidence).unwrap();

        let kinds = |rows: &[HealthFindingRow]| -> Vec<String> {
            rows.iter().map(|r| r.biomarker_kind.to_string()).collect()
        };
        assert_eq!(kinds(&active), ["function_hotspot"]);
        assert_eq!(kinds(&waived), ["code_age_volatility"]);
    }
}
