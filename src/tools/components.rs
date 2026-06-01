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
    let all_anchors = db.all_anchors_grouped()?;
    let mut items = Vec::new();
    for c in &components {
        let files = db.component_file_paths(&c.id)?;
        let anchors: Vec<serde_json::Value> = all_anchors
            .get(&c.id)
            .map(|a| {
                a.iter()
                    .map(|row| {
                        json!({
                            "symbol": row.symbol_name,
                            "score": row.score,
                            "rationale": row.rationale,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        items.push(json!({
            "id": c.id,
            "name": c.name,
            "files": files,
            "file_count": files.len(),
            "anchors": anchors,
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
