use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindArgs {
    pub workspace: String,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}
use crate::error::Result;
use crate::freshness::{self, FreshnessCounts};

pub fn handle(
    db: &Db,
    name: &str,
    kind: Option<&str>,
    limit: Option<i64>,
) -> Result<serde_json::Value> {
    handle_with_freshness(db, name, kind, limit, None)
}

pub fn handle_with_freshness(
    db: &Db,
    name: &str,
    kind: Option<&str>,
    limit: Option<i64>,
    workspace_root: Option<&Path>,
) -> Result<serde_json::Value> {
    let limit = limit.unwrap_or(10);
    let (results, tier) = db.find_symbols_by_name_tiered(name, kind, limit)?;

    let mut counts = FreshnessCounts::default();
    let items: Vec<_> = results
        .iter()
        .map(|s| {
            let file_path = db.file_by_id(s.file_id).ok().flatten();
            let file_str = file_path.as_ref().map(|f| f.path.as_str());
            let mut entry = json!({
                "id": s.id,
                "qualified_name": s.qualified_name,
                "short_name": s.short_name,
                "kind": s.kind,
                "file": file_str,
                "start_line": s.start_line,
                "end_line": s.end_line,
                "signature": s.signature,
                "visibility": s.visibility,
            });
            if let (Some(root), Some(fp)) = (workspace_root, &file_path) {
                let status = freshness::check_file(root, &fp.path, &fp.last_parsed);
                counts.record(status);
                entry["_freshness"] = json!(status.as_str());
            }
            entry
        })
        .collect();

    let mut result = json!({ "matches": items, "total": items.len() });
    if workspace_root.is_some() {
        result["_meta"] = json!({
            "freshness": counts.to_json(),
            "confidence": freshness::confidence_json(tier),
        });
    }
    Ok(result)
}
