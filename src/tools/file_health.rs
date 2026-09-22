use std::collections::HashSet;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::FreshnessAnnotator;
use crate::health::assess::{self, FileEvidence, PersistentEvidence};
use crate::health::evidence::Validity;
use crate::health::scoring::{self, FileHealthScore, MissingProducer, ScoreValue};
use crate::tools::scoring::round3;

use super::ToolContext;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileHealthArgs {
    #[serde(default)]
    pub workspace: String,
    #[serde(default, alias = "file")]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// "actionable" (default): only files with health findings. "all": every file.
    #[serde(default)]
    pub mode: Option<String>,
    /// Filter to files belonging to this component (by name).
    #[serde(default)]
    pub component: Option<String>,
    /// When true, include `_explain` with raw deductions, category caps, and scale factors.
    #[serde(default)]
    pub explain: Option<bool>,
}

/// File health over the current run, read under the caller-established
/// `validity` (sutra/416). Callers that did not just refresh should pass the
/// result of `refresh::current_run_validity`.
pub fn handle(
    db: &Db,
    validity: Validity,
    path: Option<&str>,
    limit: Option<i64>,
    mode: Option<&str>,
    component: Option<&str>,
    explain: bool,
) -> Result<serde_json::Value> {
    handle_inner(db, validity, path, limit, mode, component, None, explain)
}

pub fn handle_ctx(
    ctx: &ToolContext,
    refresh: crate::health::refresh::DemandOutcome,
    path: Option<&str>,
    limit: Option<i64>,
    mode: Option<&str>,
    component: Option<&str>,
    explain: bool,
) -> Result<serde_json::Value> {
    let mut result = handle_inner(
        ctx.db(),
        refresh.persistent_validity(),
        path,
        limit,
        mode,
        component,
        ctx.freshness_annotator(),
        explain,
    )?;
    // Component membership is only recomputed by a full parse; the demand refresh
    // that ran before this call rebuilt file rollups but did not re-cluster. If the
    // stored membership is no longer current for the live graph/history/config, the
    // component scores `handle_inner` just built (and their instability penalty) are
    // computed off a stale grouping — mark them unavailable rather than presenting a
    // possibly-wrong grouping as current. This is a distinct axis from the per-file
    // evidence, which is genuinely current after the refresh (sutra/426).
    if result.get("components").is_some()
        && !crate::components::membership_current(ctx.db(), ctx.workspace_root())?
        && let Some(obj) = result.as_object_mut()
    {
        obj.remove("components");
        obj.remove("total_components");
        obj.insert(
            "components_unavailable".into(),
            json!({
                "reason": "stale_membership",
                "detail": "component clustering is stale relative to the current \
                    graph/history/config; run a full parse to refresh component scores",
            }),
        );
    }
    Ok(result)
}

/// Attach a `health_evidence` block summarising the demand-refresh outcome and
/// the coherent published run's validity/partiality (sutra/415 Wave D). Consumers
/// call this after `SutraServer::refresh_health` so the report states whether the
/// scores it just read are current, deferred, or partial — rather than presenting
/// possibly-stale numbers as authoritative. The scores themselves come from the
/// live tables `publish_run` just refreshed; this only annotates their provenance.
pub fn attach_health_evidence(
    db: &Db,
    result: &mut serde_json::Value,
    outcome: crate::health::refresh::DemandOutcome,
) -> Result<()> {
    use crate::health::evidence::ProducerOutcome;

    // Shared validity mapping (single source of truth on DemandOutcome) so the
    // file-health evidence stamp and the review delta gate agree token-for-token.
    let validity = outcome.validity();

    let mut evidence = json!({ "validity": validity });
    match db.load_current_health_run()? {
        Some(run) => {
            evidence["run_id"] = json!(run.id.0);
            // Distinct producers whose current evidence is Missing => partial run.
            // Unsupported (structurally N/A) and Complete are not partiality.
            let mut seen: HashSet<&'static str> = HashSet::new();
            let mut missing: Vec<serde_json::Value> = Vec::new();
            for o in &run.outcomes {
                if let ProducerOutcome::Missing(reason) = &o.outcome
                    && seen.insert(o.producer.as_str())
                {
                    missing.push(json!({
                        "producer": o.producer.as_str(),
                        "reason": serde_json::to_value(reason).unwrap_or(serde_json::Value::Null),
                    }));
                }
            }
            evidence["partial"] = json!(!missing.is_empty());
            if !missing.is_empty() {
                evidence["missing_producers"] = json!(missing);
            }
        }
        None => {
            // No run has ever been published: the scores are legacy/live-table
            // reads with no evidence stamp — never claim completeness.
            evidence["run_id"] = serde_json::Value::Null;
            evidence["partial"] = json!(true);
        }
    }

    if let Some(obj) = result.as_object_mut() {
        obj.insert("health_evidence".into(), evidence);
    }
    Ok(())
}

/// Serialize a score value: a measured score is a number; a partial one has a
/// `null` point score plus its bounds — never a point value presented as a
/// measurement (health-evidence-contract.md § Comparison and scoring).
pub(crate) fn score_value_json(value: &ScoreValue) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    match *value {
        ScoreValue::Measured(s) => {
            map.insert("health_score".into(), json!(scoring::round2(s)));
        }
        ScoreValue::Partial { lower, upper } => {
            map.insert("health_score".into(), serde_json::Value::Null);
            map.insert(
                "score_bounds".into(),
                json!({ "lower": scoring::round2(lower), "upper": scoring::round2(upper) }),
            );
            map.insert("partial".into(), json!(true));
        }
    }
    map
}

/// `[{biomarker, reason}]` for a score's missing producers.
pub(crate) fn missing_json(missing: &[MissingProducer]) -> serde_json::Value {
    json!(
        missing
            .iter()
            .map(|m| json!({
                "biomarker": m.biomarker.as_str(),
                "reason": serde_json::to_value(m.reason).unwrap_or(serde_json::Value::Null),
            }))
            .collect::<Vec<_>>()
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "private seam shared by the ctx and plain entry points; each argument is an independent filter"
)]
fn handle_inner(
    db: &Db,
    validity: Validity,
    path: Option<&str>,
    limit: Option<i64>,
    mode: Option<&str>,
    component: Option<&str>,
    mut annotator: Option<FreshnessAnnotator<'_>>,
    explain: bool,
) -> Result<serde_json::Value> {
    let limit = limit.unwrap_or(20) as usize;
    let mode = mode.unwrap_or("actionable");

    let evidence = PersistentEvidence::load(db, validity)?;

    // Resolve component filter to a set of file IDs
    let component_file_ids: Option<HashSet<i64>> = if let Some(comp_name) = component {
        let comp_name_lower = comp_name.to_lowercase();
        let comps = db.active_components_with_paths()?;
        let matched = comps
            .iter()
            .find(|(_, name, _)| name.to_lowercase() == comp_name_lower);
        match matched {
            Some((comp_id, _, _)) => {
                let ids = db.component_file_ids(comp_id)?;
                Some(ids.into_iter().collect())
            }
            None => {
                return Ok(json!({
                    "error": format!("no component found matching '{}'", comp_name),
                    "hint": "use sutra_components to list available components",
                }));
            }
        }
    } else {
        None
    };

    let in_scope = |f: &FileEvidence| -> bool {
        if let Some(p) = path
            && f.path != p
        {
            return false;
        }
        if let Some(ref ids) = component_file_ids
            && !ids.contains(&f.file_id)
        {
            return false;
        }
        true
    };

    let mut scored: Vec<(&FileEvidence, FileHealthScore)> = evidence
        .files
        .iter()
        .filter(|f| in_scope(f))
        .filter(|f| mode != "actionable" || !f.findings.is_empty())
        .map(|f| (f, f.score()))
        .collect();

    // Worst known debt first, then the widest uncertainty.
    scored.sort_by(|a, b| {
        let key = |s: &FileHealthScore| (s.value.upper(), s.value.lower());
        key(&a.1)
            .partial_cmp(&key(&b.1))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);

    let items: Vec<_> = scored
        .iter()
        .map(|(f, score)| {
            let mut entry = file_entry(f, score, explain);
            if let Some(ref mut ann) = annotator
                && let Some(row) = db.file_by_path(&f.path).ok().flatten()
            {
                ann.annotate_file(&mut entry, &f.path, &row.last_parsed);
            }
            entry
        })
        .collect();

    let mut result = json!({
        "files": items,
        "total_files": items.len(),
        "mode": mode,
    });

    // Surface dimensions that are structurally unmeasurable in this workspace
    // (data source absent) rather than silently omitting them — an unsupported
    // biomarker is excluded from scoring, not scored as zero debt.
    let unsupported: Vec<serde_json::Value> = evidence
        .unsupported()
        .iter()
        .map(|(k, reason)| json!({ "biomarker": k.as_str(), "reason": reason.as_str() }))
        .collect();
    if !unsupported.is_empty() {
        result["unsupported_biomarkers"] = json!(unsupported);
    }

    if path.is_none() && component.is_none() {
        let components = build_component_scores(db, &evidence)?;
        result["total_components"] = json!(components.len());
        result["components"] = json!(components);
    }

    if let Some(ann) = annotator {
        result["_meta"] = json!({ "freshness": ann.finish() });
    }
    Ok(result)
}

/// One file's report entry. Findings of a producer without current evidence are
/// listed as `stale` with no deduction: retained for visibility, never counted.
fn file_entry(f: &FileEvidence, score: &FileHealthScore, explain: bool) -> serde_json::Value {
    let deduction_of = |i: usize| {
        score
            .deductions
            .iter()
            .find(|d| d.part == 0 && d.index == i)
    };
    let findings_json: Vec<_> = f
        .findings
        .iter()
        .enumerate()
        .map(|(i, finding)| {
            let mut j = json!({
                "biomarker": finding.biomarker_kind,
                "severity": finding.severity,
                "metric_value": finding.metric_value,
                "threshold": finding.threshold,
                "detail": finding.detail,
            });
            match deduction_of(i) {
                Some(d) => j["deduction"] = json!(scoring::round2(d.scaled_deduction)),
                None => {
                    j["deduction"] = json!(0.0);
                    j["stale"] = json!(true);
                }
            }
            j
        })
        .collect();

    let cat_json: serde_json::Map<String, serde_json::Value> = score
        .categories
        .iter()
        .filter(|c| c.known > 0.0)
        .map(|c| {
            (
                c.category.as_str().to_string(),
                json!(scoring::round2(c.known)),
            )
        })
        .collect();

    let mut entry = score_value_json(&score.value);
    entry.insert("path".into(), json!(f.path));
    entry.insert("category_deductions".into(), cat_json.into());
    entry.insert("findings".into(), json!(findings_json));
    if !score.missing.is_empty() {
        entry.insert("missing_biomarkers".into(), json!(score.missing_names()));
        entry.insert("missing".into(), missing_json(&score.missing));
    }
    if explain {
        let categories_explain: serde_json::Map<String, serde_json::Value> = score
            .categories
            .iter()
            .map(|c| {
                let cap = c.category.cap();
                (
                    c.category.as_str().to_string(),
                    json!({
                        "cap": cap,
                        "raw_total": round3(c.known_raw),
                        "capped": c.known_raw > cap,
                        "scale_factor": if c.known_raw > cap { round3(cap / c.known_raw) } else { 1.0 },
                        "pessimistic_deduction": round3(c.pessimistic),
                    }),
                )
            })
            .collect();
        let findings_explain: Vec<_> = score
            .deductions
            .iter()
            .filter(|d| d.part == 0)
            .map(|d| {
                json!({
                    "biomarker": f.findings[d.index].biomarker_kind,
                    "raw_deduction": round3(d.raw_deduction),
                    "scaled_deduction": round3(d.scaled_deduction),
                    "scale_factor": if d.raw_deduction > 0.0 { round3(d.scaled_deduction / d.raw_deduction) } else { 1.0 },
                })
            })
            .collect();
        entry.insert(
            "_explain".into(),
            json!({
                "formula": "upper = 10.0 - sum(capped known deductions); lower additionally \
                    saturates every category with a missing producer; clamped to [1.0, 10.0]",
                "categories": categories_explain,
                "findings": findings_explain,
            }),
        );
    }
    serde_json::Value::Object(entry)
}

fn build_component_scores(
    db: &Db,
    evidence: &PersistentEvidence,
) -> Result<Vec<serde_json::Value>> {
    let workspace = assess::score_workspace(db, evidence)?;

    let mut comp_results: Vec<(f64, serde_json::Value)> = workspace
        .components
        .iter()
        .map(|cs| {
            let mut entry = score_value_json(&cs.value);
            entry.insert("id".into(), json!(cs.component_id));
            entry.insert("name".into(), json!(cs.component_name));
            entry.insert("member_count".into(), json!(cs.member_count));
            entry.insert("total_nloc".into(), json!(cs.total_nloc));
            if let Some(inst) = &cs.instability {
                entry.insert(
                    "instability".into(),
                    json!({
                        "ce": inst.ce,
                        "ca": inst.ca,
                        "value": scoring::round2(inst.instability),
                    }),
                );
            }
            (cs.value.upper(), serde_json::Value::Object(entry))
        })
        .collect();

    comp_results.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(comp_results.into_iter().map(|(_, v)| v).collect())
}
