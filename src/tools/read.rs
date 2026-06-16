use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::lessons::LessonsDb;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    pub workspace: String,
    #[schemars(
        description = "Symbol name to read. Pass a qualified name (e.g. \"evaluate_dd\", \
        \"GuardConfig::from_env\") or a short name (e.g. \"from_env\"). \
        Do NOT prefix with file paths or extensions — \"build_findings\" not \"review.rs::build_findings\"."
    )]
    pub symbol: String,
    #[serde(default)]
    pub context_lines: Option<usize>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub full: Option<bool>,
}
use crate::error::{Result, SutraError};

const DEFAULT_LINE_CAP: usize = 500;

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    symbol: &str,
    context_lines: Option<usize>,
    limit: Option<usize>,
    full: bool,
    is_stale: bool,
    lessons_db: Option<&LessonsDb>,
) -> Result<serde_json::Value> {
    let context_lines = context_lines.unwrap_or(5);
    let line_cap = if full {
        usize::MAX
    } else {
        limit.unwrap_or(DEFAULT_LINE_CAP)
    };

    let sym = db.resolve_symbol(symbol, None)?.ok_or_else(|| {
        let next_action = if let Some(suggestion) = diagnose_symbol_input(symbol) {
            suggestion
        } else {
            "Use sutra_find to look up the symbol name first.".to_string()
        };
        SutraError::NotFound {
            tool: "sutra_read",
            kind: format!("symbol `{symbol}`"),
            next_action,
        }
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

    // Tier-2 freshness: withhold content when index is stale
    if is_stale {
        return Ok(json!({
            "symbol": sym.qualified_name,
            "file": file.path,
            "start_line": sym.start_line,
            "end_line": sym.end_line,
            "kind": sym.kind,
            "signature": sym.signature,
            "refused": "content withheld: index is stale",
            "next_action": "Run sutra_parse to refresh, then retry.",
        }));
    }

    let source = std::fs::read_to_string(&abs_path)?;
    let lines: Vec<&str> = source.lines().collect();

    let sym_start = (sym.start_line as usize).saturating_sub(1);
    let sym_end = std::cmp::min(sym.end_line as usize, lines.len());
    let sym_lines = sym_end - sym_start;

    let (start, end, truncated) = if sym_lines >= line_cap {
        (sym_start, sym_start + line_cap, true)
    } else {
        let context_budget = line_cap.saturating_sub(sym_lines);
        let pre = std::cmp::min(context_lines, context_budget / 2);
        let post = std::cmp::min(context_lines, context_budget - pre);
        let s = sym_start.saturating_sub(pre);
        let e = std::cmp::min(sym_end + post, lines.len());
        (s, e, false)
    };
    let total_lines = sym_lines + 2 * context_lines;

    let numbered: Vec<_> = lines[start..end]
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>5} {}", start + i + 1, line))
        .collect();

    let mut result = json!({
        "symbol": sym.qualified_name,
        "file": file.path,
        "start_line": start + 1,
        "end_line": end,
        "content": numbered.join("\n"),
        "kind": sym.kind,
        "signature": sym.signature,
    });
    if truncated {
        result["truncated"] = json!(true);
        result["total_lines"] = json!(total_lines);
        result["hint"] = json!(format!(
            "Truncated at {} lines ({} total). Call with full=true or limit={} to see more.",
            line_cap, total_lines, total_lines
        ));
    }

    if let Some(ldb) = lessons_db {
        let project_slug = workspace_root.file_name().and_then(|n| n.to_str());
        let lessons = ldb.query_for_context(&sym.qualified_name, project_slug)?;
        if !lessons.is_empty() {
            result["lessons"] = serde_json::to_value(&lessons).unwrap_or_default();
        }
    }

    Ok(result)
}

fn diagnose_symbol_input(symbol: &str) -> Option<String> {
    // "review.rs::build_findings" or "check.rs::evaluate"
    if let Some(pos) = symbol.find(".rs::") {
        let after = &symbol[pos + 5..];
        return Some(format!(
            "Don't prefix with the file name — pass the symbol name directly: \
             try `{after}` instead of `{symbol}`."
        ));
    }
    // "src/tools/review.rs::build_findings"
    if symbol.contains('/') {
        let bare = symbol.rsplit("::").next().unwrap_or(symbol);
        return Some(format!(
            "Don't use file paths — pass the symbol name directly: \
             try `{bare}` instead of `{symbol}`."
        ));
    }
    // "review.rs" (just a filename, no symbol)
    if symbol.ends_with(".rs")
        || symbol.ends_with(".ts")
        || symbol.ends_with(".py")
        || symbol.ends_with(".dart")
    {
        return Some(format!(
            "`{symbol}` looks like a file path, not a symbol name. \
             Pass a function or type name (e.g. `build_findings`). \
             Use sutra_outline to list symbols in a file."
        ));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnose_file_dot_rs_prefix() {
        let msg = diagnose_symbol_input("review.rs::build_findings").unwrap();
        assert!(msg.contains("try `build_findings`"));
    }

    #[test]
    fn diagnose_path_prefix() {
        let msg = diagnose_symbol_input("src/tools/review.rs::build_findings").unwrap();
        assert!(msg.contains("try `build_findings`"));
    }

    #[test]
    fn diagnose_bare_filename() {
        let msg = diagnose_symbol_input("review.rs").unwrap();
        assert!(msg.contains("looks like a file path"));
    }

    #[test]
    fn no_diagnosis_for_valid_symbol() {
        assert!(diagnose_symbol_input("build_findings").is_none());
        assert!(diagnose_symbol_input("GuardConfig::from_env").is_none());
    }
}
