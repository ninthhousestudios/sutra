use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use crate::error::{Result, SutraError};

pub fn git_diff_files(workspace_root: &Path, base: &str, head: &str) -> Result<Vec<String>> {
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
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args([
            "log",
            "--name-only",
            "--pretty=format:COMMIT_SEP",
            "--since",
        ])
        .arg(&since)
        .arg("--")
        .arg(path)
        .output()
        .map_err(|e| SutraError::Internal(format!("git log failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git log: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut counts: HashMap<String, u32> = HashMap::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() || line == "COMMIT_SEP" || line == path {
            continue;
        }
        *counts.entry(line.to_string()).or_insert(0) += 1;
    }

    let mut result: Vec<(String, u32)> = counts.into_iter().collect();
    result.sort_by_key(|&(_, count)| std::cmp::Reverse(count));

    Ok(result)
}

/// Count how many commits touched each file in the given time window.
pub fn git_churn(workspace_root: &Path, window_days: u32) -> Result<HashMap<String, u32>> {
    let since = format!("{window_days} days ago");
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["log", "--format=", "--name-only", "--no-renames", "--since"])
        .arg(&since)
        .output()
        .map_err(|e| SutraError::Internal(format!("git log failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git log: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut counts: HashMap<String, u32> = HashMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.is_empty() {
            *counts.entry(line.to_string()).or_insert(0) += 1;
        }
    }
    Ok(counts)
}
