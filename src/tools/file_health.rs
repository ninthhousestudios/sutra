use std::path::Path;

use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::{self, FreshnessCounts};

pub fn handle(
    db: &Db,
    path: Option<&str>,
    limit: Option<i64>,
) -> Result<serde_json::Value> {
    handle_with_freshness(db, path, limit, None)
}

pub fn handle_with_freshness(
    db: &Db,
    path: Option<&str>,
    limit: Option<i64>,
    workspace_root: Option<&Path>,
) -> Result<serde_json::Value> {
    let limit = limit.unwrap_or(20) as usize;
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
            let (_, avg_cog) = complexity.get(&f.id).copied().unwrap_or((0, 0.0));
            let dead_ratio = dead_ratios.get(&f.id).copied().unwrap_or(0.0);
            let pr_norm = f.pagerank.unwrap_or(0.0) / max_pr;

            let blast_penalty = (f.blast_radius * 2).min(25) as f64;
            let complexity_penalty = (avg_cog * 2.0).min(25.0);
            let fan_in_penalty = (f.fan_in_files).min(15) as f64;
            let dead_penalty = dead_ratio * 20.0;
            let pr_penalty = (pr_norm * 15.0).min(15.0);

            let total_penalty =
                blast_penalty + complexity_penalty + fan_in_penalty + dead_penalty + pr_penalty;
            const MAX_PENALTY: f64 = 100.0;
            let health = (MAX_PENALTY - total_penalty).max(0.0) as i64;

            (f, health, blast_penalty, complexity_penalty, fan_in_penalty, dead_penalty, pr_penalty, avg_cog, dead_ratio)
        })
        .collect();

    scored.sort_by_key(|e| e.1);
    scored.truncate(limit);

    let mut counts = FreshnessCounts::default();
    let items: Vec<_> = scored
        .iter()
        .map(
            |(f, health, blast_p, comp_p, fan_p, dead_p, pr_p, avg_cog, dead_ratio)| {
                let mut entry = json!({
                    "path": f.path,
                    "health_score": health,
                    "breakdown": {
                        "blast_radius_penalty": *blast_p as i64,
                        "complexity_penalty": *comp_p as i64,
                        "fan_in_penalty": *fan_p as i64,
                        "dead_symbol_penalty": *dead_p as i64,
                        "pagerank_penalty": *pr_p as i64,
                    },
                    "raw": {
                        "blast_radius": f.blast_radius,
                        "avg_cognitive": avg_cog,
                        "fan_in_files": f.fan_in_files,
                        "dead_symbol_ratio": dead_ratio,
                        "pagerank": f.pagerank,
                    },
                });
                if let Some(root) = workspace_root {
                    let status = freshness::check_file(root, &f.path, &f.last_parsed);
                    counts.record(status);
                    entry["_freshness"] = json!(status.as_str());
                }
                entry
            },
        )
        .collect();

    let mut result = json!({
        "files": items,
        "total": items.len(),
    });
    if workspace_root.is_some() {
        result["_meta"] = json!({ "freshness": counts.to_json() });
    }
    Ok(result)
}
