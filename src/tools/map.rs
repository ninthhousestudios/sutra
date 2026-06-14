use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::FreshnessAnnotator;

use super::ToolContext;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MapArgs {
    pub workspace: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    /// When true, include `_explain` with scoring rationale.
    #[serde(default)]
    pub explain: Option<bool>,
}

pub fn handle(
    db: &Db,
    path_prefix: Option<&str>,
    limit: Option<i64>,
    explain: bool,
) -> Result<serde_json::Value> {
    handle_inner(db, path_prefix, limit, None, explain)
}

pub fn handle_ctx(
    ctx: &ToolContext,
    path_prefix: Option<&str>,
    limit: Option<i64>,
    explain: bool,
) -> Result<serde_json::Value> {
    handle_inner(
        ctx.db(),
        path_prefix,
        limit,
        ctx.freshness_annotator(),
        explain,
    )
}

fn handle_inner(
    db: &Db,
    path_prefix: Option<&str>,
    limit: Option<i64>,
    mut annotator: Option<FreshnessAnnotator<'_>>,
    explain: bool,
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
            (
                f,
                symbol_count,
                importance,
                max_cog,
                avg_cog,
                pr_boost,
                complexity_boost,
            )
        })
        .collect();

    entries.sort_by_key(|e| std::cmp::Reverse(e.2));
    entries.truncate(limit as usize);

    let items: Vec<_> = entries
        .iter()
        .map(
            |(f, sym_count, importance, max_cog, avg_cog, pr_boost, complexity_boost)| {
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
                if explain {
                    entry["_explain"] = json!({
                        "importance_breakdown": {
                            "symbol_count": sym_count,
                            "fan_in_boost": f.fan_in_files * 2,
                            "blast_radius": f.blast_radius,
                            "pagerank_boost": pr_boost,
                            "complexity_boost": complexity_boost,
                        }
                    });
                }
                if let Some(ref mut ann) = annotator {
                    ann.annotate_file(&mut entry, &f.path, &f.last_parsed);
                }
                entry
            },
        )
        .collect();

    let mut result = json!({ "files": items, "total": items.len() });
    if explain {
        result["_explain"] = json!({
            "formula": "symbol_count + fan_in_files*2 + blast_radius + floor(pagerank*1000) + min(max_cognitive, 20)"
        });
    }
    if let Some(ann) = annotator {
        result["_meta"] = json!({ "freshness": ann.finish() });
    }
    Ok(result)
}
