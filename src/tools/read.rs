use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    pub workspace: String,
    pub symbol: String,
    #[serde(default)]
    pub context_lines: Option<usize>,
}
use crate::error::{Result, SutraError};

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    symbol: &str,
    context_lines: Option<usize>,
) -> Result<serde_json::Value> {
    let context_lines = context_lines.unwrap_or(5);

    let sym = db
        .resolve_symbol(symbol, None)?
        .ok_or_else(|| SutraError::NotFound {
            tool: "sutra_read",
            kind: format!("symbol `{symbol}`"),
            next_action: "Use sutra_find to look up the symbol name first.".to_string(),
        })?;

    let file = db
        .file_by_id(sym.file_id)?
        .ok_or_else(|| SutraError::NotFound {
            tool: "sutra_read",
            kind: format!("file for symbol `{symbol}`"),
            next_action: "The file may have been deleted. Run sutra_parse to refresh.".to_string(),
        })?;

    let abs_path = workspace_root.join(&file.path);

    if !abs_path.starts_with(workspace_root) {
        return Err(SutraError::InvalidArgument {
            tool: "sutra_read",
            argument: "symbol",
            constraint: "file path must stay within workspace root".to_string(),
            received: Some(file.path.clone()),
            next_action: "This file path contains path traversal sequences. Report this issue."
                .to_string(),
        });
    }

    if !abs_path.exists() {
        return Ok(json!({
            "symbol": sym.qualified_name,
            "file": file.path,
            "start_line": sym.start_line,
            "end_line": sym.end_line,
            "warning": "file deleted since last parse",
            "is_stale": true,
            "signature": sym.signature,
            "kind": sym.kind,
        }));
    }

    let source = std::fs::read_to_string(&abs_path)?;
    let lines: Vec<&str> = source.lines().collect();

    let start = (sym.start_line as usize)
        .saturating_sub(1)
        .saturating_sub(context_lines);
    let end = std::cmp::min(
        (sym.end_line as usize).saturating_sub(1) + context_lines + 1,
        lines.len(),
    );

    let numbered: Vec<_> = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>5} {}", start + i + 1, line))
        .collect();

    Ok(json!({
        "symbol": sym.qualified_name,
        "file": file.path,
        "start_line": start + 1,
        "end_line": end,
        "content": numbered.join("\n"),
        "kind": sym.kind,
        "signature": sym.signature,
    }))
}
