use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::git;
use crate::tools::symbol_diff;

const MAX_COMMITS: usize = 50;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CommitManifestArgs {
    #[serde(default)]
    pub workspace: String,
    /// Base commit (default: merge-base with default branch)
    #[serde(default)]
    pub base: Option<String>,
    /// Head commit (default: HEAD)
    #[serde(default)]
    pub head: Option<String>,
}

pub fn handle(
    _db: &Db,
    workspace_root: &Path,
    base: Option<&str>,
    head: Option<&str>,
) -> Result<serde_json::Value> {
    let head = head.unwrap_or("HEAD");
    let base = match base {
        Some(b) => b.to_string(),
        None => {
            let default_branch = git::detect_default_branch(workspace_root)?;
            git::git_merge_base(workspace_root, &default_branch)?
        }
    };

    let all_commits = git::git_list_commits(workspace_root, &base, head)?;
    let truncated = all_commits.len() > MAX_COMMITS;
    let commits = &all_commits[..all_commits.len().min(MAX_COMMITS)];

    let mut entries = Vec::with_capacity(commits.len());
    for commit in commits {
        let parent = format!("{}~1", commit.hash);
        let diff_entries = match git::git_diff_files(workspace_root, &parent, &commit.hash) {
            Ok(entries) => entries,
            Err(e) => {
                entries.push(json!({
                    "hash": commit.hash,
                    "subject": commit.subject,
                    "author": commit.author,
                    "timestamp": commit.timestamp,
                    "diff_error": e.to_string(),
                    "files": [],
                }));
                continue;
            }
        };

        let mut file_entries = Vec::with_capacity(diff_entries.len());
        for de in &diff_entries {
            let mut entry = json!({ "path": de.path });
            match symbol_diff::diff_file(
                workspace_root,
                &de.path,
                de.old_path.as_deref(),
                &parent,
                &commit.hash,
            ) {
                Ok(sc) if !sc.is_empty() => {
                    entry["symbol_changes"] = serde_json::to_value(&sc).unwrap_or_default();
                }
                Ok(_) => {}
                Err(e) => {
                    entry["symbol_diff_error"] = json!(e.to_string());
                }
            }
            file_entries.push(entry);
        }

        entries.push(json!({
            "hash": commit.hash,
            "subject": commit.subject,
            "author": commit.author,
            "timestamp": commit.timestamp,
            "files": file_entries,
        }));
    }

    Ok(json!({
        "base": base,
        "head": head,
        "commit_count": entries.len(),
        "truncated": truncated,
        "commits": entries,
    }))
}
