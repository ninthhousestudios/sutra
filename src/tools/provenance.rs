use std::path::Path;
use std::process::Command;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProvenanceArgs {
    pub workspace: String,
    pub symbol: String,
}
use crate::error::{Result, SutraError};

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    pub author: String,
    pub date: String,
    pub message: String,
}

pub fn classify(message: &str) -> &'static str {
    let lower = message.to_lowercase();
    let prefix = lower.split(':').next().unwrap_or("");
    let tag = prefix.split('(').next().unwrap_or("").trim();

    match tag {
        "feat" | "feature" => "feature",
        "fix" => "bugfix",
        "refactor" => "refactor",
        "test" | "tests" => "test",
        "doc" | "docs" => "docs",
        "perf" => "performance",
        "chore" | "ci" | "build" | "style" => "chore",
        _ => "unknown",
    }
}

pub fn compute(symbol: &str, file: &str, commits: &[CommitInfo]) -> serde_json::Value {
    let entries: Vec<_> = commits
        .iter()
        .map(|c| {
            json!({
                "sha": c.sha,
                "author": c.author,
                "date": c.date,
                "message": c.message,
                "classification": classify(&c.message),
            })
        })
        .collect();

    json!({
        "symbol": symbol,
        "file": file,
        "commits": entries,
        "total": entries.len(),
    })
}

pub fn handle(db: &Db, workspace_root: &Path, symbol: &str) -> Result<serde_json::Value> {
    let sym = db
        .resolve_symbol(symbol, None)?
        .ok_or_else(|| SutraError::NotFound {
            tool: "sutra_provenance",
            kind: format!("symbol `{symbol}`"),
            next_action: "Check the symbol name and try sutra_find to search.".into(),
        })?;

    let file = db
        .file_by_id(sym.file_id)?
        .ok_or_else(|| SutraError::NotFound {
            tool: "sutra_provenance",
            kind: format!("file for symbol `{symbol}`"),
            next_action: "The symbol's file is missing from the index. Run sutra_parse.".into(),
        })?;

    let commits = git_log_follow(workspace_root, &file.path)?;

    Ok(compute(&sym.qualified_name, &file.path, &commits))
}

fn git_log_follow(workspace_root: &Path, path: &str) -> Result<Vec<CommitInfo>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["log", "--follow", "--format=%H\x1f%an\x1f%aI\x1f%s", "--"])
        .arg(path)
        .output()
        .map_err(|e| SutraError::Internal(format!("git log failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git log: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut commits = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(4, '\x1f').collect();
        if parts.len() == 4 {
            commits.push(CommitInfo {
                sha: parts[0].to_string(),
                author: parts[1].to_string(),
                date: parts[2].to_string(),
                message: parts[3].to_string(),
            });
        }
    }

    // git log returns newest-first; reverse for chronological
    commits.reverse();
    Ok(commits)
}
