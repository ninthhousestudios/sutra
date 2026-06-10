use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::FreshnessAnnotator;
use crate::git;

use super::ToolContext;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HotspotsArgs {
    pub workspace: String,
    #[serde(default)]
    pub window_days: Option<u32>,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    window_days: Option<u32>,
    limit: Option<i64>,
) -> Result<serde_json::Value> {
    handle_inner(db, workspace_root, window_days, limit, None)
}

pub fn handle_ctx(
    ctx: &ToolContext,
    window_days: Option<u32>,
    limit: Option<i64>,
) -> Result<serde_json::Value> {
    handle_inner(
        ctx.db(),
        ctx.workspace_root(),
        window_days,
        limit,
        ctx.freshness_annotator(),
    )
}

fn handle_inner(
    db: &Db,
    workspace_root: &Path,
    window_days: Option<u32>,
    limit: Option<i64>,
    mut annotator: Option<FreshnessAnnotator<'_>>,
) -> Result<serde_json::Value> {
    let window = window_days.unwrap_or(90);
    let limit = limit.unwrap_or(20) as usize;

    let churn = git::git_churn(workspace_root, window)?;
    let files = db.all_files()?;
    let complexity = db.complexity_by_file()?;

    if files.is_empty() {
        return Ok(json!({ "window_days": window, "hotspots": [], "total": 0 }));
    }

    let max_churn = churn.values().copied().max().unwrap_or(1).max(1) as f64;
    let max_blast = files
        .iter()
        .map(|f| f.blast_radius)
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    let max_complexity = complexity
        .values()
        .map(|(max_c, _)| *max_c)
        .max()
        .unwrap_or(1)
        .max(1) as f64;

    let mut scored: Vec<_> = files
        .iter()
        .filter_map(|f| {
            let c = churn.get(&f.path).copied().unwrap_or(0);
            if c == 0 {
                return None;
            }
            let (max_cog, _) = complexity.get(&f.id).copied().unwrap_or((0, 0.0));
            let churn_norm = c as f64 / max_churn;
            let blast_norm = f.blast_radius as f64 / max_blast;
            let complexity_norm = (max_cog as f64 / max_complexity).max(0.1);
            let score = (churn_norm * blast_norm * complexity_norm * 1000.0) as i64;
            Some((f, c, max_cog, score))
        })
        .collect();

    scored.sort_by_key(|e| std::cmp::Reverse(e.3));
    scored.truncate(limit);

    let items: Vec<_> = scored
        .iter()
        .map(|(f, churn_count, max_cog, score)| {
            let mut entry = json!({
                "path": f.path,
                "score": score,
                "churn": churn_count,
                "blast_radius": f.blast_radius,
                "max_cognitive": max_cog,
                "pagerank": f.pagerank,
            });
            if let Some(ref mut ann) = annotator {
                ann.annotate_file(&mut entry, &f.path, &f.last_parsed);
            }
            entry
        })
        .collect();

    let mut result = json!({
        "window_days": window,
        "hotspots": items,
        "total": items.len(),
    });
    if let Some(ann) = annotator {
        result["_meta"] = json!({ "freshness": ann.finish() });
    }
    Ok(result)
}
