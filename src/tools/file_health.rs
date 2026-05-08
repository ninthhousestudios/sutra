use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, FileRow};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileHealthArgs {
    pub workspace: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// "actionable" (default): omits foundational files (high coupling but low complexity and \
    /// no dead code). "all": shows every file.
    #[serde(default)]
    pub mode: Option<String>,
}
use crate::error::Result;
use crate::freshness::{self, FreshnessCounts};

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
    let files = db.all_files()?;
    let complexity = db.complexity_by_file()?;
    let dead_ratios = db.dead_symbol_ratio_by_file()?;

    let max_pr = files
        .iter()
        .filter_map(|f| f.pagerank)
        .fold(0.0_f64, f64::max)
        .max(0.001);

    let mut scored: Vec<_> = files
        .iter()
        .filter(|f| match path {
            Some(p) => f.path == p,
            None => true,
        })
        .map(|f| {
            let (max_cog, avg_cog) = complexity.get(&f.id).copied().unwrap_or((0, 0.0));
            let dead_ratio = dead_ratios.get(&f.id).copied().unwrap_or(0.0);
            let scores = compute_file_scores(f, max_cog, avg_cog, dead_ratio, max_pr);
            (f, scores)
        })
        .collect();

    if mode == "actionable" {
        scored.retain(|(_, s)| !(s.complexity_health >= 90.0 && s.dead_health >= 90.0));
    }

    scored.sort_by(|a, b| {
        a.1.overall_health
            .partial_cmp(&b.1.overall_health)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);

    let mut counts = FreshnessCounts::default();
    let items: Vec<_> = scored
        .iter()
        .map(|(f, s)| {
            let mut entry = json!({
                "path": f.path,
                "health_score": s.overall_health as i64,
                "complexity_health": s.complexity_health as i64,
                "dead_health": s.dead_health as i64,
                "coupling_health": s.coupling_health as i64,
                "raw": {
                    "blast_radius": f.blast_radius,
                    "avg_cognitive": s.avg_cognitive,
                    "max_cognitive": s.max_cognitive,
                    "fan_in_files": f.fan_in_files,
                    "dead_symbol_ratio": s.dead_ratio,
                    "pagerank": f.pagerank,
                },
            });
            if let Some(root) = workspace_root {
                let status = freshness::check_file(root, &f.path, &f.last_parsed);
                counts.record(status);
                entry["_freshness"] = json!(status.as_str());
            }
            entry
        })
        .collect();

    let mut result = json!({
        "files": items,
        "total": items.len(),
        "mode": mode,
    });
    if workspace_root.is_some() {
        result["_meta"] = json!({ "freshness": counts.to_json() });
    }
    Ok(result)
}
