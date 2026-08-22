use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::FreshnessAnnotator;

use super::ToolContext;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindArgs {
    #[serde(default)]
    pub workspace: String,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub detail: Option<bool>,
}

pub fn handle(
    db: &Db,
    name: &str,
    kind: Option<&str>,
    limit: Option<i64>,
    detail: bool,
) -> Result<serde_json::Value> {
    handle_inner(db, name, kind, limit, detail, None)
}

pub fn handle_ctx(
    ctx: &ToolContext,
    name: &str,
    kind: Option<&str>,
    limit: Option<i64>,
    detail: bool,
) -> Result<serde_json::Value> {
    handle_inner(
        ctx.db(),
        name,
        kind,
        limit,
        detail,
        ctx.freshness_annotator(),
    )
}

fn handle_inner(
    db: &Db,
    name: &str,
    kind: Option<&str>,
    limit: Option<i64>,
    detail: bool,
    mut annotator: Option<FreshnessAnnotator<'_>>,
) -> Result<serde_json::Value> {
    let limit = limit.unwrap_or(10);
    let (results, tier) = db.find_symbols_by_name_tiered(name, kind, limit)?;

    let items: Vec<_> = results
        .iter()
        .map(|s| {
            let file_path = db.file_by_id(s.file_id).ok().flatten();
            let file_str = file_path.as_ref().map(|f| &*f.path);
            let mut entry = json!({
                "qualified_name": s.qualified_name,
                "kind": s.kind,
                "file": file_str,
                "start_line": s.start_line,
                "end_line": s.end_line,
            });
            if detail {
                entry["id"] = json!(s.id);
                entry["short_name"] = json!(s.short_name);
                entry["signature"] = json!(s.signature);
                entry["visibility"] = json!(s.visibility);
            }
            if let (Some(ann), Some(fp)) = (&mut annotator, &file_path) {
                ann.annotate_file(&mut entry, &fp.path, &fp.last_parsed);
            }
            entry
        })
        .collect();

    let has_annotator = annotator.is_some();
    let mut result = json!({ "matches": items, "total": items.len() });
    if let Some(ref ann) = annotator {
        result["_meta"] = json!({
            "freshness": ann.counts().to_json(),
            "confidence": tier.confidence_json(),
        });
    }
    if items.is_empty() {
        let indexed_kinds = db.distinct_symbol_kinds().unwrap_or_default();
        let freshness_level = if let Some(ann) = &annotator {
            let counts = ann.counts();
            if counts.stale > 0 {
                Some(crate::freshness::FreshnessLevel::StaleIndex)
            } else if counts.edited > 0 {
                Some(crate::freshness::FreshnessLevel::EditedUncommitted)
            } else {
                Some(crate::freshness::FreshnessLevel::Fresh)
            }
        } else if has_annotator {
            Some(crate::freshness::FreshnessLevel::Fresh)
        } else {
            None
        };
        result["diagnostic"] = serde_json::to_value(crate::diagnostics::Diagnostic::NoSuchSymbol {
            queried_name: name.to_string(),
            queried_kind: kind.map(String::from),
            indexed_kinds,
            freshness: freshness_level,
            suggestion: "Try sutra_lookup to match symbol names by pattern, \
                             or verify the exact symbol name with sutra_map."
                .to_string(),
        })
        .unwrap();
    }
    Ok(result)
}
