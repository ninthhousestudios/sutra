use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use crate::error::{Result, SutraError};

#[derive(Debug, Clone)]
pub struct DiffFileEntry {
    pub path: String,
    pub old_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BlameLine {
    pub commit: String,
    pub author_time: i64,
    pub line_no: usize,
}

pub fn parse_blame_porcelain(input: &str) -> Vec<BlameLine> {
    let mut results = Vec::new();
    let mut current_commit = String::new();
    let mut current_time: i64 = 0;
    let mut current_line: usize = 0;
    let mut time_cache: HashMap<String, i64> = HashMap::new();

    for line in input.lines() {
        if line.starts_with('\t') {
            results.push(BlameLine {
                commit: current_commit.clone(),
                author_time: current_time,
                line_no: current_line,
            });
        } else if let Some(ts) = line.strip_prefix("author-time ") {
            current_time = ts.trim().parse().unwrap_or(0);
            time_cache.insert(current_commit.clone(), current_time);
        } else {
            let bytes = line.as_bytes();
            if bytes.len() > 40
                && bytes[40] == b' '
                && bytes[..40].iter().all(|b| b.is_ascii_hexdigit())
            {
                current_commit = line[..40].to_string();
                if let Some(&cached) = time_cache.get(&current_commit) {
                    current_time = cached;
                }
                let parts: Vec<&str> = line[41..].split_whitespace().collect();
                if parts.len() >= 2 {
                    current_line = parts[1].parse().unwrap_or(0);
                }
            }
        }
    }
    results
}

pub fn git_blame_porcelain(workspace_root: &Path, path: &str) -> Result<Vec<BlameLine>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["blame", "--porcelain", path])
        .output()
        .map_err(|e| SutraError::Internal(format!("git blame failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no such path") || stderr.contains("bad revision") {
            return Ok(vec![]);
        }
        return Err(SutraError::Internal(format!("git blame: {stderr}")));
    }

    let text = String::from_utf8(output.stdout)
        .map_err(|e| SutraError::Internal(format!("git blame: non-UTF8: {e}")))?;
    Ok(parse_blame_porcelain(&text))
}

pub struct CommitFile {
    pub hash: String,
    pub timestamp: i64,
    pub author: String,
    pub path: String,
}

pub fn git_diff_files(workspace_root: &Path, base: &str, head: &str) -> Result<Vec<DiffFileEntry>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["diff", "--name-status", "-M"])
        .arg(format!("{base}..{head}"))
        .output()
        .map_err(|e| SutraError::Internal(format!("git diff failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git diff: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        let parts: Vec<&str> = line.split('\t').collect();
        match parts.as_slice() {
            [status, old, new] if status.starts_with('R') || status.starts_with('C') => {
                entries.push(DiffFileEntry {
                    path: new.to_string(),
                    old_path: Some(old.to_string()),
                });
            }
            [_status, path] => {
                entries.push(DiffFileEntry {
                    path: path.to_string(),
                    old_path: None,
                });
            }
            _ => {}
        }
    }
    Ok(entries)
}

pub fn git_diff_staged(workspace_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["diff", "--name-only", "--cached"])
        .output()
        .map_err(|e| SutraError::Internal(format!("git diff --cached failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git diff --cached: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

pub fn git_diff_unstaged(workspace_root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["diff", "--name-only"])
        .output()
        .map_err(|e| SutraError::Internal(format!("git diff failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git diff: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

pub fn detect_default_branch(workspace_root: &Path) -> Result<String> {
    // Try remote HEAD symbolic-ref first
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .output()
        .ok();

    if let Some(ref out) = output
        && out.status.success()
    {
        let refname = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if let Some(branch) = refname.strip_prefix("refs/remotes/origin/") {
            return Ok(branch.to_string());
        }
    }

    // Fall back to checking local branches
    for candidate in &["main", "master"] {
        let check = Command::new("git")
            .arg("-C")
            .arg(workspace_root)
            .args(["rev-parse", "--verify", candidate])
            .output()
            .ok();
        if let Some(ref out) = check
            && out.status.success()
        {
            return Ok(candidate.to_string());
        }
    }

    Err(SutraError::Internal(
        "cannot detect default branch: no remote HEAD, and neither 'main' nor 'master' exist"
            .into(),
    ))
}

pub fn git_merge_base(workspace_root: &Path, branch: &str) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["merge-base", "HEAD", branch])
        .output()
        .map_err(|e| SutraError::Internal(format!("git merge-base failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git merge-base: {stderr}")));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub struct CommitEntry {
    pub hash: String,
    pub timestamp: i64,
    pub author: String,
    pub subject: String,
}

pub fn git_list_commits(workspace_root: &Path, base: &str, head: &str) -> Result<Vec<CommitEntry>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args([
            "log",
            "--first-parent",
            "--no-merges",
            "--reverse",
            "--format=%H %at %ae %s",
        ])
        .arg(format!("{base}..{head}"))
        .output()
        .map_err(|e| SutraError::Internal(format!("git log failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git log: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(4, ' ').collect();
        if parts.len() == 4 {
            results.push(CommitEntry {
                hash: parts[0].to_string(),
                timestamp: parts[1].parse().unwrap_or(0),
                author: parts[2].to_string(),
                subject: parts[3].to_string(),
            });
        }
    }

    Ok(results)
}

/// Return all (commit_hash, timestamp, author, file_path) tuples from git
/// history within the given window. One entry per file per commit.
pub fn git_commit_files(workspace_root: &Path, window_days: u32) -> Result<Vec<CommitFile>> {
    let since = format!("{window_days} days ago");
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args([
            "log",
            "--format=COMMIT_SEP %H %at %ae",
            "--name-only",
            "--no-renames",
            "--since",
        ])
        .arg(&since)
        .output()
        .map_err(|e| SutraError::Internal(format!("git log failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git log: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    let mut current_hash = String::new();
    let mut current_ts: i64 = 0;
    let mut current_author = String::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("COMMIT_SEP ") {
            let parts: Vec<&str> = rest.splitn(3, ' ').collect();
            if parts.len() == 3 {
                current_hash = parts[0].to_string();
                current_ts = parts[1].parse().unwrap_or(0);
                current_author = parts[2].to_string();
            }
            continue;
        }
        if !current_hash.is_empty() {
            results.push(CommitFile {
                hash: current_hash.clone(),
                timestamp: current_ts,
                author: current_author.clone(),
                path: line.to_string(),
            });
        }
    }

    Ok(results)
}

pub fn churn_from_commit_files(commit_files: &[CommitFile]) -> HashMap<String, u32> {
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut seen: HashSet<(&str, &str)> = HashSet::new();
    for cf in commit_files {
        if seen.insert((&cf.hash, &cf.path)) {
            *counts.entry(cf.path.clone()).or_default() += 1;
        }
    }
    counts
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

pub fn git_file_content_at(
    workspace_root: &Path,
    revision: &str,
    path: &str,
) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["show", &format!("{revision}:{path}")])
        .output()
        .map_err(|e| SutraError::Internal(format!("git show failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("does not exist")
            || stderr.contains("exists on disk, but not in")
            || stderr.contains("bad revision")
        {
            return Ok(None);
        }
        return Err(SutraError::Internal(format!("git show: {stderr}")));
    }

    String::from_utf8(output.stdout)
        .map(Some)
        .map_err(|e| SutraError::Internal(format!("git show: non-UTF8 content: {e}")))
}

pub fn head_commit_hash(workspace_root: &Path) -> Option<String> {
    Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

pub struct FirstParentCommit {
    pub hash: String,
    pub timestamp: i64,
    pub author: String,
    pub is_merge: bool,
}

pub fn git_first_parent_commits(
    workspace_root: &Path,
    max_commits: u32,
) -> Result<Vec<FirstParentCommit>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args([
            "rev-list",
            "--first-parent",
            "--parents",
            "--format=%at %ae",
            "--max-count",
        ])
        .arg(max_commits.to_string())
        .arg("HEAD")
        .output()
        .map_err(|e| SutraError::Internal(format!("git rev-list failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git rev-list: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();
    let mut current_hash = String::new();
    let mut current_is_merge = false;

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("commit ") {
            // Format: "<hash> <parent1> [<parent2> ...]"
            let hashes: Vec<&str> = rest.split_whitespace().collect();
            current_hash = hashes.first().unwrap_or(&"").to_string();
            current_is_merge = hashes.len() > 2; // hash + 2+ parents
        } else if !current_hash.is_empty() {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() == 2 {
                results.push(FirstParentCommit {
                    hash: std::mem::take(&mut current_hash),
                    timestamp: parts[0].parse().unwrap_or(0),
                    author: parts[1].to_string(),
                    is_merge: current_is_merge,
                });
            }
        }
    }

    Ok(results)
}

pub fn git_commit_changed_files(
    workspace_root: &Path,
    commit_hash: &str,
) -> Result<Vec<DiffFileEntry>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace_root)
        .args(["diff-tree", "--no-commit-id", "-r", "-M", "--name-status"])
        .arg(commit_hash)
        .output()
        .map_err(|e| SutraError::Internal(format!("git diff-tree failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(SutraError::Internal(format!("git diff-tree: {stderr}")));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        let parts: Vec<&str> = line.split('\t').collect();
        match parts.as_slice() {
            [status, old, new] if status.starts_with('R') || status.starts_with('C') => {
                entries.push(DiffFileEntry {
                    path: new.to_string(),
                    old_path: Some(old.to_string()),
                });
            }
            [_status, path] => {
                entries.push(DiffFileEntry {
                    path: path.to_string(),
                    old_path: None,
                });
            }
            _ => {}
        }
    }
    Ok(entries)
}
