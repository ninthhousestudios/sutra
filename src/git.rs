use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crate::error::{Result, SutraError};

pub fn git_diff_files(
    workspace_root: &Path,
    base: &str,
    head: &str,
) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["diff", "--name-only"])
        .arg(format!("{base}..{head}"))
        .output()
        .map_err(|e| SutraError::Internal(format!("git diff failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git diff: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let files = stdout
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect();

    Ok(files)
}

pub fn git_cochange_files(
    workspace_root: &Path,
    path: &str,
    window_days: u32,
) -> Result<Vec<(String, u32)>> {
    let since = format!("{window_days} days ago");
    let commits_output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["log", "--format=%H", "--since"])
        .arg(&since)
        .arg("--")
        .arg(path)
        .output()
        .map_err(|e| SutraError::Internal(format!("git log failed: {e}")))?;

    if !commits_output.status.success() {
        let stderr = String::from_utf8_lossy(&commits_output.stderr);
        return Err(SutraError::Internal(format!("git log: {stderr}")));
    }

    let commits_str = String::from_utf8_lossy(&commits_output.stdout);
    let commit_hashes: Vec<&str> = commits_str
        .lines()
        .filter(|l| !l.is_empty())
        .collect();

    let mut counts: HashMap<String, u32> = HashMap::new();

    for hash in commit_hashes {
        let show_output = Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .args(["show", "--name-only", "--format="])
            .arg(hash)
            .output()
            .map_err(|e| SutraError::Internal(format!("git show failed: {e}")))?;

        if !show_output.status.success() {
            continue;
        }

        let show_str = String::from_utf8_lossy(&show_output.stdout);
        for line in show_str.lines().filter(|l| !l.is_empty()) {
            if line != path {
                *counts.entry(line.to_string()).or_insert(0) += 1;
            }
        }
    }

    let mut result: Vec<(String, u32)> = counts.into_iter().collect();
    result.sort_by_key(|&(_, count)| std::cmp::Reverse(count));

    Ok(result)
}
