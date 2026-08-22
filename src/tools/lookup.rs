use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use std::collections::HashSet;

use crate::db::{Db, SearchTier};
use crate::error::Result;
use crate::freshness::FreshnessAnnotator;

use super::ToolContext;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LookupArgs {
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

    // Support grep-style alternation: "A|B|C" matches symbols named like any of
    // the terms. Each term is looked up independently and the rows are unioned
    // (deduped by symbol id, first-seen order preserved). A single term (no `|`)
    // takes the plain path so the exact-match tier is reported faithfully.
    let terms: Vec<&str> = pattern
        .split('|')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();

    let (results, tier) = if terms.len() > 1 {
        let mut seen: HashSet<i64> = HashSet::new();
        let mut merged = Vec::new();
        // A union across fuzzy terms is inherently broad; only claim the exact
        // tier when every term that produced rows produced them exactly.
        let mut all_exact = true;
        for term in &terms {
            let (rows, term_tier) = db.find_symbols_by_name_tiered(term, kind, limit)?;
            if !rows.is_empty() && term_tier != SearchTier::Exact {
                all_exact = false;
            }
            for row in rows {
                if seen.insert(row.id) {
                    merged.push(row);
                }
            }
        }
        merged.truncate(limit as usize);
        let tier = if all_exact && !merged.is_empty() {
            SearchTier::Exact
        } else {
            SearchTier::Fts
        };
        (merged, tier)
    } else {
        db.find_symbols_by_name_tiered(pattern, kind, limit)?
    };

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
    if items.is_empty() {
        // Distinguish "no symbol by that name" from "wrong tool": callers reach
        // for this expecting text/regex grep and get a silent empty otherwise.
        result["hint"] = json!(
            "No symbol matched by name. sutra_lookup searches symbol names \
             (definitions), not file text — and `|` is the only regex-style \
             operator it honors (alternation). For text, comments, or string \
             literals use rg; for usages/call sites of a known symbol use \
             sutra_refs or sutra_calls; for fuzzy or topic search use sutra_explore."
        );
    }
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
    use super::LookupArgs;

    // An omitted `workspace` must deserialize (empty string → session default is
    // applied downstream in resolve_workspace) rather than being a hard error.
    #[test]
    fn workspace_may_be_omitted_when_deserializing() {
        let args: LookupArgs = serde_json::from_value(serde_json::json!({
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
        let schema = schemars::schema_for!(LookupArgs);
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
