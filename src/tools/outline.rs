use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OutlineArgs {
    pub workspace: String,
    pub path: String,
    /// If true, return only structural fields. Default is false (full detail).
    #[serde(default)]
    pub compact: Option<bool>,
}

use crate::error::{Result, SutraError};

pub fn handle(db: &Db, path: &str, compact: bool) -> Result<serde_json::Value> {
    let file = db.file_by_path(path)?.ok_or_else(|| SutraError::NotFound {
        tool: "sutra_outline",
        kind: format!("file `{path}`"),
        next_action: "Check the path and try again. Use sutra_map to list available files."
            .to_string(),
    })?;

    let symbols = db.find_symbols_by_file(file.id)?;

    let items: Vec<_> = symbols
        .iter()
        .map(|s| {
            if compact {
                json!({
                    "qualified_name": s.qualified_name,
                    "kind": s.kind,
                    "start_line": s.start_line,
                    "end_line": s.end_line,
                    "visibility": s.visibility,
                })
            } else {
                let mut entry = json!({
                    "qualified_name": s.qualified_name,
                    "short_name": s.short_name,
                    "kind": s.kind,
                    "start_line": s.start_line,
                    "end_line": s.end_line,
                    "signature": s.signature,
                    "visibility": s.visibility,
                    "parent_symbol_id": s.parent_symbol_id,
                    "docstring": s.docstring,
                });
                if let Some(c) = s.cyclomatic {
                    entry["cyclomatic"] = json!(c);
                }
                if let Some(c) = s.cognitive {
                    entry["cognitive"] = json!(c);
                }
                entry
            }
        })
        .collect();

    Ok(json!({
        "path": file.path,
        "language": file.language,
        "symbols": items,
        "total": items.len(),
    }))
}
