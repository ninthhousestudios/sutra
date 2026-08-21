use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::{Result, SutraError};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConventionsArgs {
    #[serde(default)]
    pub workspace: String,
    /// Action: "list"
    pub action: String,
}

pub fn handle(db: &Db, args: &ConventionsArgs) -> Result<serde_json::Value> {
    match args.action.as_str() {
        "list" => handle_list(db),
        other => Err(SutraError::Internal(format!(
            "unknown action: {other}. expected: list"
        ))),
    }
}

fn handle_list(db: &Db) -> Result<serde_json::Value> {
    let conventions = db.all_conventions()?;

    let conventions_out: Vec<_> = conventions
        .iter()
        .map(|c| {
            json!({
                "id": c.id,
                "antecedent": c.antecedent,
                "consequent": c.consequent,
                "support": c.support,
                "confidence": c.confidence,
                "component_id": c.component_id,
            })
        })
        .collect();

    Ok(json!({ "conventions": conventions_out }))
}
