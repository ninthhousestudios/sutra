use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, FileRow, HealthFindingRow};
use crate::error::Result;
use crate::freshness::{self, FreshnessCounts};
use crate::health::scoring;

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
}

// Legacy scoring — still used by pipeline.rs::compute_snapshot_aggregates.
// Replaced by health::scoring for the MCP tool; will be removed when sutra/91
// (health snapshots) lands and rewires the pipeline.

pub struct FileScores {
    pub complexity_health: f64,
    pub dead_health: f64,
    pub coupling_health: f64,
    pub overall_health: f64,
    pub avg_cognitive: f64,
    pub max_cognitive: i64,
    pub dead_ratio: f64,
}

pub fn compute_file_scores(
    f: &FileRow,
    max_cognitive: i64,
    avg_cognitive: f64,
    dead_ratio: f64,
    max_pagerank: f64,
) -> FileScores {
    let complexity_health =
        (100.0 - (avg_cognitive * 4.0).min(60.0) - (max_cognitive as f64 * 0.5).min(40.0)).max(0.0);

    let dead_health = (100.0 - dead_ratio * 100.0).max(0.0);

    let pr_norm = f.pagerank.unwrap_or(0.0) / max_pagerank;
    let blast_scaled = (f.blast_radius as f64 * 1.5).min(40.0);
    let fan_in_scaled = (f.fan_in_files as f64 * 1.0).min(30.0);
    let pr_scaled = (pr_norm * 30.0).min(30.0);
    let coupling_health = (100.0 - blast_scaled - fan_in_scaled - pr_scaled).max(0.0);

    let overall_health = 0.45 * complexity_health + 0.30 * dead_health + 0.25 * coupling_health;

    FileScores {
        complexity_health,
        dead_health,
        coupling_health,
        overall_health,
        avg_cognitive,
        max_cognitive,
        dead_ratio,
    }
}

pub fn handle(
    db: &Db,
    path: Option<&str>,
    limit: Option<i64>,
    mode: Option<&str>,
) -> Result<serde_json::Value> {
    handle_with_freshness(db, path, limit, mode, None)
}

pub fn handle_with_freshness(
    db: &Db,
    path: Option<&str>,
    limit: Option<i64>,
    mode: Option<&str>,
    workspace_root: Option<&Path>,
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

    let target_files: Vec<&FileRow> = if mode == "actionable" {
        findings_by_file
            .keys()
            .filter_map(|fid| file_map.get(fid).copied())
            .filter(|f| match path {
                Some(p) => f.path == p,
                None => true,
            })
            .collect()
    } else {
        files
            .iter()
            .filter(|f| match path {
                Some(p) => f.path == p,
                None => true,
            })
            .collect()
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

            let refs = findings_by_file
                .get(&file.id)
                .map(|v| v.clone())
                .unwrap_or_default();

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

    let mut counts = FreshnessCounts::default();
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
                        "deduction": round2(ded),
                        "detail": f.detail,
                    })
                })
                .collect();

            let cat_json: serde_json::Value = s
                .deductions
                .iter()
                .map(|(&k, &v)| (k.to_string(), json!(round2(v))))
                .collect::<serde_json::Map<String, serde_json::Value>>()
                .into();

            let mut entry = json!({
                "path": s.file.path,
                "health_score": round2(s.score),
                "category_deductions": cat_json,
                "findings": findings_json,
            });
            if let Some(root) = workspace_root {
                let status = freshness::check_file(root, &s.file.path, &s.file.last_parsed);
                counts.record(status);
                entry["_freshness"] = json!(status.as_str());
            }
            entry
        })
        .collect();

    let mut result = json!({
        "files": items,
        "total_files": items.len(),
        "mode": mode,
    });

    if path.is_none() {
        if let Ok(components) = build_component_scores(db, &findings_by_file) {
            result["components"] = json!(components);
            result["total_components"] = json!(components.len());
        }
    }

    if workspace_root.is_some() {
        result["_meta"] = json!({ "freshness": counts.to_json() });
    }
    Ok(result)
}

fn build_component_scores(
    db: &Db,
    findings_by_file: &HashMap<i64, Vec<&HealthFindingRow>>,
) -> Result<Vec<serde_json::Value>> {
    let components = db.all_components()?;
    let memberships = db.component_members_with_line_count()?;

    let mut comp_files: HashMap<&str, Vec<(i64, i64)>> = HashMap::new();
    for (comp_id, file_id, line_count) in &memberships {
        comp_files
            .entry(comp_id.as_str())
            .or_default()
            .push((*file_id, *line_count));
    }

    let mut comp_results = Vec::new();
    for comp in &components {
        let members = match comp_files.get(comp.id.as_str()) {
            Some(m) => m,
            None => continue,
        };

        let file_scores: Vec<(f64, i64)> = members
            .iter()
            .map(|&(file_id, line_count)| {
                let findings: Vec<HealthFindingRow> = findings_by_file
                    .get(&file_id)
                    .map(|refs| refs.iter().map(|r| (*r).clone()).collect())
                    .unwrap_or_default();
                let score = scoring::score_file(&findings).score;
                (score, line_count)
            })
            .collect();

        let total_nloc: i64 = file_scores.iter().map(|(_, n)| n).sum();
        let comp_score = scoring::score_component(&file_scores);

        comp_results.push(json!({
            "id": comp.id,
            "name": comp.name,
            "health_score": round2(comp_score),
            "member_count": members.len(),
            "total_nloc": total_nloc,
        }));
    }

    comp_results.sort_by(|a, b| {
        let sa = a["health_score"].as_f64().unwrap_or(10.0);
        let sb = b["health_score"].as_f64().unwrap_or(10.0);
        sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(comp_results)
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
