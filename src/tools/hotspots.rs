use std::path::Path;

use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::{self, FreshnessCounts};
use crate::git;

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    window_days: Option<u32>,
    limit: Option<i64>,
) -> Result<serde_json::Value> {
    handle_with_freshness(db, workspace_root, window_days, limit, false)
}

pub fn handle_with_freshness(
    db: &Db,
    workspace_root: &Path,
    window_days: Option<u32>,
    limit: Option<i64>,
    include_freshness: bool,
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

    let mut counts = FreshnessCounts::default();
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
            if include_freshness {
                let status = freshness::check_file(workspace_root, &f.path, &f.last_parsed);
                counts.record(status);
                entry["_freshness"] = json!(status.as_str());
            }
            entry
        })
        .collect();

    let mut result = json!({
        "window_days": window,
        "hotspots": items,
        "total": items.len(),
    });
    if include_freshness {
        result["_meta"] = json!({ "freshness": counts.to_json() });
    }
    Ok(result)
}
