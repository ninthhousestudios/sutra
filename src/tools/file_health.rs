use std::collections::{HashMap, HashSet};

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, FileRow, HealthFindingRow};
use crate::error::Result;
use crate::freshness::FreshnessAnnotator;
use crate::health::scoring;

use super::ToolContext;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileHealthArgs {
    pub workspace: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// "actionable" (default): only files with health findings. "all": every file.
    #[serde(default)]
    pub mode: Option<String>,
    /// Filter to files belonging to this component (by name).
    #[serde(default)]
    pub component: Option<String>,
}

pub fn handle(
    db: &Db,
    path: Option<&str>,
    limit: Option<i64>,
    mode: Option<&str>,
    component: Option<&str>,
) -> Result<serde_json::Value> {
    handle_inner(db, path, limit, mode, component, None)
}

pub fn handle_ctx(
    ctx: &ToolContext,
    path: Option<&str>,
    limit: Option<i64>,
    mode: Option<&str>,
    component: Option<&str>,
) -> Result<serde_json::Value> {
    handle_inner(
        ctx.db(),
        path,
        limit,
        mode,
        component,
        ctx.freshness_annotator(),
    )
}

fn handle_inner(
    db: &Db,
    path: Option<&str>,
    limit: Option<i64>,
    mode: Option<&str>,
    component: Option<&str>,
    mut annotator: Option<FreshnessAnnotator<'_>>,
) -> Result<serde_json::Value> {
    let limit = limit.unwrap_or(20) as usize;
    let mode = mode.unwrap_or("actionable");

    let all_with_waivers = db.get_health_findings_with_waiver_status()?;
    let active: Vec<HealthFindingRow> = all_with_waivers
        .into_iter()
        .filter(|(_, waived)| !waived)
        .map(|(f, _)| f)
        .collect();

    let files = db.all_files()?;
    let file_map: HashMap<i64, &FileRow> = files.iter().map(|f| (f.id, f)).collect();

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

    let mut findings_by_file: HashMap<i64, Vec<&HealthFindingRow>> = HashMap::new();
    for f in &active {
        findings_by_file.entry(f.file_id).or_default().push(f);
    }

    struct ScoredFile<'a> {
        file: &'a FileRow,
        score: f64,
        deductions: HashMap<&'static str, f64>,
        findings: Vec<&'a HealthFindingRow>,
        finding_deductions: Vec<f64>,
    }

    let in_scope = |f: &FileRow| -> bool {
        if let Some(p) = path
            && f.path != p
        {
            return false;
        }
        if let Some(ref ids) = component_file_ids
            && !ids.contains(&f.id)
        {
            return false;
        }
        true
    };

    let target_files: Vec<&FileRow> = if mode == "actionable" {
        findings_by_file
            .keys()
            .filter_map(|fid| file_map.get(fid).copied())
            .filter(|f| in_scope(f))
            .collect()
    } else {
        files.iter().filter(|f| in_scope(f)).collect()
    };

    let mut scored: Vec<ScoredFile> = target_files
        .into_iter()
        .map(|file| {
            let file_findings: Vec<HealthFindingRow> = findings_by_file
                .get(&file.id)
                .map(|refs| refs.iter().map(|r| (*r).clone()).collect())
                .unwrap_or_default();

            let result = scoring::score_file(&file_findings);

            let mut cat_totals: HashMap<&'static str, f64> = HashMap::new();
            let mut finding_deductions = vec![0.0_f64; file_findings.len()];
            for d in &result.deductions {
                *cat_totals.entry(d.category.as_str()).or_default() += d.scaled_deduction;
                if let Some(pos) = file_findings.iter().position(|f| f.id == d.finding_id) {
                    finding_deductions[pos] = d.scaled_deduction;
                }
            }

            let refs = findings_by_file.get(&file.id).cloned().unwrap_or_default();

            ScoredFile {
                file,
                score: result.score,
                deductions: cat_totals,
                findings: refs,
                finding_deductions,
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);

    let items: Vec<_> = scored
        .iter()
        .map(|s| {
            let findings_json: Vec<_> = s
                .findings
                .iter()
                .zip(s.finding_deductions.iter())
                .map(|(f, &ded)| {
                    json!({
                        "biomarker": f.biomarker_kind,
                        "severity": f.severity,
                        "metric_value": f.metric_value,
                        "threshold": f.threshold,
                        "deduction": scoring::round2(ded),
                        "detail": f.detail,
                    })
                })
                .collect();

            let cat_json: serde_json::Value = s
                .deductions
                .iter()
                .map(|(&k, &v)| (k.to_string(), json!(scoring::round2(v))))
                .collect::<serde_json::Map<String, serde_json::Value>>()
                .into();

            let mut entry = json!({
                "path": s.file.path,
                "health_score": scoring::round2(s.score),
                "category_deductions": cat_json,
                "findings": findings_json,
            });
            if let Some(ref mut ann) = annotator {
                ann.annotate_file(&mut entry, &s.file.path, &s.file.last_parsed);
            }
            entry
        })
        .collect();

    let mut result = json!({
        "files": items,
        "total_files": items.len(),
        "mode": mode,
    });

    if path.is_none()
        && component.is_none()
        && let Ok(components) = build_component_scores(db)
    {
        result["components"] = json!(components);
        result["total_components"] = json!(components.len());
    }

    if let Some(ann) = annotator {
        result["_meta"] = json!({ "freshness": ann.finish() });
    }
    Ok(result)
}

fn build_component_scores(db: &Db) -> Result<Vec<serde_json::Value>> {
    let workspace = scoring::score_workspace(db, true)?;

    let mut comp_results: Vec<serde_json::Value> = workspace
        .component_scores
        .iter()
        .map(|cs| {
            let mut entry = json!({
                "id": cs.component_id,
                "name": cs.component_name,
                "health_score": scoring::round2(cs.score),
                "member_count": cs.member_count,
                "total_nloc": cs.total_nloc,
            });

            if let Some(inst) = &cs.instability {
                entry["instability"] = json!({
                    "ce": inst.ce,
                    "ca": inst.ca,
                    "value": scoring::round2(inst.instability),
                });
            }

            entry
        })
        .collect();

    comp_results.sort_by(|a, b| {
        let sa = a["health_score"].as_f64().unwrap_or(10.0);
        let sb = b["health_score"].as_f64().unwrap_or(10.0);
        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(comp_results)
}
