use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::FreshnessAnnotator;

use super::ToolContext;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepArgs {
    #[serde(default)]
    pub workspace: String,
    pub pattern: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub detail: Option<bool>,
}

pub fn handle(
    db: &Db,
    pattern: &str,
    kind: Option<&str>,
    limit: Option<i64>,
    detail: bool,
) -> Result<serde_json::Value> {
    handle_inner(db, pattern, kind, limit, detail, None)
}

pub fn handle_ctx(
    ctx: &ToolContext,
    pattern: &str,
    kind: Option<&str>,
    limit: Option<i64>,
    detail: bool,
) -> Result<serde_json::Value> {
    handle_inner(
        ctx.db(),
        pattern,
        kind,
        limit,
        detail,
        ctx.freshness_annotator(),
    )
}

fn handle_inner(
    db: &Db,
    pattern: &str,
    kind: Option<&str>,
    limit: Option<i64>,
    detail: bool,
    mut annotator: Option<FreshnessAnnotator<'_>>,
) -> Result<serde_json::Value> {
    let limit = limit.unwrap_or(20);
    let (results, tier) = db.find_symbols_by_name_tiered(pattern, kind, limit)?;

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
                entry["docstring"] = json!(s.docstring);
            }
            if let (Some(ann), Some(fp)) = (&mut annotator, &file_path) {
                ann.annotate_file(&mut entry, &fp.path, &fp.last_parsed);
            }
            entry
        })
        .collect();

    let mut result = json!({ "matches": items, "total": items.len() });
    if let Some(ann) = annotator {
        result["_meta"] = json!({
            "freshness": ann.finish(),
            "confidence": tier.confidence_json(),
        });
    }
    Ok(result)
}

#[cfg(test)]
mod optional_workspace_tests {
    use super::GrepArgs;

    // An omitted `workspace` must deserialize (empty string → session default is
    // applied downstream in resolve_workspace) rather than being a hard error.
    #[test]
    fn workspace_may_be_omitted_when_deserializing() {
        let args: GrepArgs = serde_json::from_value(serde_json::json!({
            "pattern": "foo",
        }))
        .expect("workspace should be optional at the deser layer");
        assert_eq!(args.workspace, "");
        assert_eq!(args.pattern, "foo");
    }

    // schemars must drop `workspace` from `required` (so MCP clients may omit it)
    // while genuinely-required fields like `pattern` stay required.
    #[test]
    fn schema_marks_workspace_optional_pattern_required() {
        let schema = schemars::schema_for!(GrepArgs);
        let value = serde_json::to_value(&schema).unwrap();
        let required: Vec<&str> = value["required"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        assert!(
            !required.contains(&"workspace"),
            "workspace must not be required; got {required:?}"
        );
        assert!(
            required.contains(&"pattern"),
            "pattern must stay required; got {required:?}"
        );
    }
}
