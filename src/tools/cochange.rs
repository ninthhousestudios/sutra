use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::{Result, SutraError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CochangeArgs {
    pub workspace: String,
    pub path: String,
    #[serde(default)]
    pub threshold: Option<f64>,
}

pub fn handle(db: &Db, path: &str, threshold: Option<f64>) -> Result<serde_json::Value> {
    let threshold = threshold.unwrap_or(0.1);

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
        "threshold": threshold,
        "cochanged": entries,
    }))
}
