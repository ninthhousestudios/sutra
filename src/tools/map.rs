use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MapArgs {
    pub workspace: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}
use crate::error::Result;
use crate::freshness::{self, FreshnessCounts};

pub fn handle(db: &Db, path_prefix: Option<&str>, limit: Option<i64>) -> Result<serde_json::Value> {
    handle_with_freshness(db, path_prefix, limit, None)
}

pub fn handle_with_freshness(
    db: &Db,
    path_prefix: Option<&str>,
    limit: Option<i64>,
    workspace_root: Option<&Path>,
) -> Result<serde_json::Value> {
    let limit = limit.unwrap_or(50);
    let files = db.all_files()?;
    let sym_counts = db.symbol_counts_by_file()?;
    let complexity_by_file = db.complexity_by_file()?;

    let mut entries: Vec<_> = files
        .into_iter()
        .filter(|f| match path_prefix {
            Some(prefix) => f.path.starts_with(prefix),
            None => true,
        })
        .map(|f| {
            let symbol_count = sym_counts.get(&f.id).copied().unwrap_or(0);
            let pr_boost = (f.pagerank.unwrap_or(0.0) * 1000.0) as i64;
            let (max_cog, avg_cog) = complexity_by_file.get(&f.id).copied().unwrap_or((0, 0.0));
            let complexity_boost = max_cog.min(20);
            let importance =
                symbol_count + f.fan_in_files * 2 + f.blast_radius + pr_boost + complexity_boost;
            (f, symbol_count, importance, max_cog, avg_cog)
        })
        .collect();

    entries.sort_by_key(|e| std::cmp::Reverse(e.2));
    entries.truncate(limit as usize);

    let mut counts = FreshnessCounts::default();
    let items: Vec<_> = entries
        .iter()
        .map(|(f, sym_count, importance, max_cog, avg_cog)| {
            let mut entry = json!({
                "path": f.path,
                "language": f.language,
                "line_count": f.line_count,
                "symbols": sym_count,
                "fan_in_files": f.fan_in_files,
                "blast_radius": f.blast_radius,
                "pagerank": f.pagerank,
                "importance": importance,
                "max_cognitive": max_cog,
                "avg_cognitive": avg_cog,
            });
            if let Some(root) = workspace_root {
                let status = freshness::check_file(root, &f.path, &f.last_parsed);
                counts.record(status);
                entry["_freshness"] = json!(status.as_str());
            }
            entry
        })
        .collect();

    let mut result = json!({ "files": items, "total": items.len() });
    if workspace_root.is_some() {
        result["_meta"] = json!({ "freshness": counts.to_json() });
    }
    Ok(result)
}
