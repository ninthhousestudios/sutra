use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::components::concept_density;
use crate::db::Db;
use crate::error::Result;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ComponentsArgs {
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub compact: Option<bool>,
}

pub fn handle(db: &Db, compact: bool) -> Result<serde_json::Value> {
    let components = db.all_components()?;
    let all_anchors = db.all_anchors_grouped()?;
    let all_symbols = db.all_symbols_by_file()?;
    let all_files = db.all_files()?;
    let file_loc: HashMap<i64, i64> = all_files.iter().map(|f| (f.id, f.line_count)).collect();

    let mut items = Vec::new();
    for c in &components {
        let file_ids = db.component_file_ids(&c.id)?;

        let all_component_syms: Vec<_> = file_ids
            .iter()
            .filter_map(|fid| all_symbols.get(fid))
            .flatten()
            .collect();
        let total_loc: i64 = file_ids.iter().filter_map(|fid| file_loc.get(fid)).sum();
        let density = concept_density(&all_component_syms, total_loc);

        if compact {
            let top_anchors: Vec<&str> = all_anchors
                .get(&c.id)
                .map(|a| {
                    a.iter()
                        .take(3)
                        .map(|row| row.symbol_name.as_str())
                        .collect()
                })
                .unwrap_or_default();

            items.push(json!({
                "name": c.name,
                "file_count": file_ids.len(),
                "anchors": top_anchors,
                "concept_density": density,
            }));
        } else {
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
                "concept_density": density,
            }));
        }
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
