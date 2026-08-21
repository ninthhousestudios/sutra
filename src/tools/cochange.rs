use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::{Result, SutraError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CochangeArgs {
    #[serde(default)]
    pub workspace: String,
    /// File path (file granularity) or qualified symbol name (function granularity).
    pub path: String,
    #[serde(default)]
    pub threshold: Option<f64>,
    /// "file" (default) or "function".
    #[serde(default)]
    pub granularity: Option<String>,
}

pub fn handle(
    db: &Db,
    path: &str,
    threshold: Option<f64>,
    granularity: Option<&str>,
) -> Result<serde_json::Value> {
    let threshold = threshold.unwrap_or(0.1);

    match granularity.unwrap_or("file") {
        "function" => handle_function(db, path, threshold),
        _ => handle_file(db, path, threshold),
    }
}

fn handle_file(db: &Db, path: &str, threshold: f64) -> Result<serde_json::Value> {
    let file = db
        .file_by_path(path)?
        .ok_or_else(|| SutraError::Internal(format!("file not in index: {path}")))?;

    let partners = db.cochange_for_file(file.id, threshold)?;

    let entries: Vec<serde_json::Value> = partners
        .into_iter()
        .map(|(co_path, jaccard, shared)| {
            json!({
                "path": co_path,
                "jaccard": jaccard,
                "shared_commits": shared,
            })
        })
        .collect();

    Ok(json!({
        "path": path,
        "granularity": "file",
        "threshold": threshold,
        "cochanged": entries,
    }))
}

fn handle_function(db: &Db, symbol: &str, threshold: f64) -> Result<serde_json::Value> {
    let entity_count = db.entity_change_count()?;
    if entity_count == 0 {
        return Err(SutraError::Internal(
            "entity change index is empty — run a full parse first".into(),
        ));
    }

    let partners = db.entity_cochange_for_symbol(symbol, threshold)?;

    let entries: Vec<serde_json::Value> = partners
        .into_iter()
        .map(|(name, file, jaccard, confidence, shared)| {
            json!({
                "symbol": name,
                "file": file,
                "jaccard": jaccard,
                "confidence": confidence,
                "shared_commits": shared,
            })
        })
        .collect();

    Ok(json!({
        "symbol": symbol,
        "granularity": "function",
        "threshold": threshold,
        "cochanged": entries,
    }))
}
