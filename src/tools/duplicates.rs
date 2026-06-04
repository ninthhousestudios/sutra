use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::similarity::duplicates::find_pattern_families;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DuplicatesArgs {
    pub workspace: String,
    #[serde(default)]
    pub threshold: Option<f64>,
    #[serde(default)]
    pub min_group: Option<usize>,
}

pub fn handle(
    db: &Db,
    threshold: Option<f64>,
    min_group: Option<usize>,
) -> Result<serde_json::Value> {
    let threshold = threshold.unwrap_or(0.85);
    let min_group = min_group.unwrap_or(3);

    let vectors = db.load_all_strip_vectors()?;
    let families = find_pattern_families(&vectors, threshold, min_group);

    if families.is_empty() {
        return Ok(json!({
            "families": [],
            "total": 0,
            "threshold": threshold,
            "min_group": min_group,
        }));
    }

    let sym_ids: Vec<i64> = families
        .iter()
        .flat_map(|f| &f.member_symbol_ids)
        .copied()
        .collect();
    let sym_meta = db.symbols_by_ids(&sym_ids)?;

    let family_json: Vec<serde_json::Value> = families
        .iter()
        .enumerate()
        .map(|(i, fam)| {
            let members: Vec<serde_json::Value> = fam
                .member_symbol_ids
                .iter()
                .filter_map(|&sid| {
                    sym_meta.iter().find(|s| s.id == sid).map(|s| {
                        json!({
                            "symbol": &s.qualified_name,
                            "file": &s.file_path,
                            "lines": format!("{}-{}", s.start_line, s.end_line),
                        })
                    })
                })
                .collect();
            json!({
                "family_id": i + 1,
                "member_count": fam.member_symbol_ids.len(),
                "avg_similarity": (fam.avg_similarity * 1000.0).round() / 1000.0,
                "members": members,
            })
        })
        .collect();

    Ok(json!({
        "families": family_json,
        "total": families.len(),
        "threshold": threshold,
        "min_group": min_group,
    }))
}
