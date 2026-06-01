use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComponentsArgs {
    pub workspace: String,
}

pub fn handle(db: &Db) -> Result<serde_json::Value> {
    let components = db.all_components()?;
    let mut items = Vec::new();
    for c in &components {
        let files = db.component_file_paths(&c.id)?;
        items.push(json!({
            "id": c.id,
            "name": c.name,
            "files": files,
            "file_count": files.len(),
        }));
    }
    let mut result = json!({ "components": items, "total": items.len() });
    if let Some((edge_count, file_count, clustered_at)) = db.clustering_meta_full()? {
        result["clustering"] = json!({
            "edge_count": edge_count,
            "file_count": file_count,
            "clustered_at": clustered_at,
        });
    }
    Ok(result)
}
