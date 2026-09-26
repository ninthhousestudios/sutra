//! Commit-file history ingestion (`commits` + `commit_files`).
//!
//! The ingested history feeds co-change (review `behavioral_coupling`, component
//! clustering) and the churn map semantic anchors consume. History is selected
//! against a pinned HEAD with an absolute, UTC-day-quantized committer-time
//! cutoff, never a relative `--since`.
//!
//! Callers hold the parse flock: ingestion rewrites both tables.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use tracing::warn;

use crate::db::{CommitRow, Db};
use crate::error::Result;
use crate::git;

/// Default trailing window length for git history, in days.
pub const DEFAULT_WINDOW_DAYS: u32 = 90;

const SECONDS_PER_DAY: i64 = 86_400;

/// The result of one ingestion.
pub struct HistoryIngestion {
    /// Whether an indexed file has in-window history from a complete range. When
    /// `false`, there is no current history: either it was cleared
    /// (non-repository, unborn HEAD, empty window) or the prior rows were
    /// retained because the range could not be established (probe or `git log`
    /// failure, shallow clone).
    pub loaded: bool,
    /// Per-path commit churn (only populated when `loaded`).
    pub churn: HashMap<String, u32>,
}

impl HistoryIngestion {
    fn unloaded() -> Self {
        Self {
            loaded: false,
            churn: HashMap::new(),
        }
    }
}

/// The trailing window length (days): the workspace's configured co-change
/// window, defaulting to [`DEFAULT_WINDOW_DAYS`]. A malformed `components.toml`
/// is an error, not the default (sutra/432).
pub fn window_days(workspace_root: &Path) -> Result<u32> {
    Ok(crate::components::load_config(workspace_root)?
        .cochange_window_days
        .unwrap_or(DEFAULT_WINDOW_DAYS))
}

/// Absolute committer-time cutoff for a `window_days` trailing window ending on
/// the UTC day containing `now_unix`: `floor(now/86400)*86400 - window*86400`.
/// Commits with committer time `>= cutoff` are in-window; there is no upper bound.
pub fn history_cutoff(now_unix: i64, window_days: u32) -> i64 {
    now_unix.div_euclid(SECONDS_PER_DAY) * SECONDS_PER_DAY
        - i64::from(window_days) * SECONDS_PER_DAY
}

/// Ingest commit-file history reachable from the current HEAD whose committer
/// time is inside the workspace window ending today (UTC). A confirmed
/// non-repository, an unborn HEAD or an empty window clears the tables; an
/// indeterminate failure retains the prior rows — a transient failure must not
/// destroy history.
pub fn ingest(db: &Db, workspace_root: &Path, now_unix: i64) -> Result<HistoryIngestion> {
    let cutoff = history_cutoff(now_unix, window_days(workspace_root)?);
    match git::probe_git_repo(workspace_root) {
        git::RepoProbe::ConfirmedAbsent => {
            db.replace_commit_files(&[], &[])?;
            Ok(HistoryIngestion::unloaded())
        }
        git::RepoProbe::Unknown => Ok(HistoryIngestion::unloaded()),
        git::RepoProbe::Present => ingest_present(db, workspace_root, cutoff),
    }
}

/// Ingest history for a present repository.
fn ingest_present(db: &Db, workspace_root: &Path, cutoff: i64) -> Result<HistoryIngestion> {
    let head_sha = match git::head_commit(workspace_root) {
        Ok(Some(sha)) => sha,
        Ok(None) => {
            db.replace_commit_files(&[], &[])?;
            return Ok(HistoryIngestion::unloaded());
        }
        Err(e) => {
            warn!("history: HEAD pin failed during ingestion: {e}");
            return Ok(HistoryIngestion::unloaded());
        }
    };

    // A shallow clone's object graph is truncated at the `.git/shallow`
    // boundary, so even a successful `git log` over the window cannot establish
    // that every qualifying commit is present (sutra/427). Retain prior rows.
    match git::is_shallow_repository(workspace_root) {
        Ok(true) => return Ok(HistoryIngestion::unloaded()),
        Ok(false) => {}
        Err(e) => {
            warn!("history: shallow-repository probe failed during ingestion: {e}");
            return Ok(HistoryIngestion::unloaded());
        }
    }

    match git::git_commit_files_since(workspace_root, &head_sha, cutoff) {
        // Loaded means an *indexed* file has in-window history: a window touching
        // only unindexed paths is empty, not loaded (sutra/423).
        Ok(commit_files) if !commit_files.is_empty() => {
            if write_commit_files(db, &commit_files)? == 0 {
                db.replace_commit_files(&[], &[])?;
                return Ok(HistoryIngestion::unloaded());
            }
            Ok(HistoryIngestion {
                loaded: true,
                churn: git::churn_from_commit_files(&commit_files),
            })
        }
        Ok(_) => {
            db.replace_commit_files(&[], &[])?;
            Ok(HistoryIngestion::unloaded())
        }
        Err(e) => {
            warn!("history: git commit-file ingestion failed: {e}");
            Ok(HistoryIngestion::unloaded())
        }
    }
}

/// Persist ingested commit-file rows: one `commits` row per distinct hash and one
/// `commit_files` edge per (hash, indexed file). Paths not indexed are dropped
/// from `commit_files` but still counted in the commit's `file_count`, so the
/// cochange bulk-commit cap sees a vendor sync at its real size.
fn write_commit_files(db: &Db, commit_files: &[git::CommitFile]) -> Result<usize> {
    let files = db.all_files()?;
    let path_to_id: HashMap<&str, i64> = files.iter().map(|f| (&*f.path, f.id)).collect();
    let mut paths_per_commit: HashMap<&str, HashSet<&str>> = HashMap::new();
    for cf in commit_files {
        paths_per_commit
            .entry(cf.hash.as_str())
            .or_default()
            .insert(cf.path.as_str());
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut commit_rows = Vec::new();
    for cf in commit_files {
        if seen.insert(cf.hash.as_str()) {
            let file_count = paths_per_commit
                .get(cf.hash.as_str())
                .map(|p| p.len() as i64);
            commit_rows.push(CommitRow {
                hash: cf.hash.to_string(),
                committed_at: cf.timestamp,
                author: cf.author.to_string(),
                file_count,
            });
        }
    }
    let pairs: Vec<(String, i64)> = commit_files
        .iter()
        .filter_map(|cf| {
            path_to_id
                .get(cf.path.as_str())
                .map(|&id| (cf.hash.to_string(), id))
        })
        .collect();
    db.replace_commit_files(&commit_rows, &pairs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutoff_is_whole_days_before_the_utc_midnight() {
        // 2026-09-21T14:00:00Z.
        let t = 1_758_463_200;
        let cutoff = history_cutoff(t, 90);
        assert_eq!(
            cutoff,
            t.div_euclid(SECONDS_PER_DAY) * SECONDS_PER_DAY - 90 * SECONDS_PER_DAY
        );
        assert_eq!(cutoff % SECONDS_PER_DAY, 0);
    }

    #[test]
    fn cutoff_floors_pre_epoch_timestamps() {
        assert_eq!(history_cutoff(-1, 0), -SECONDS_PER_DAY);
        assert_eq!(history_cutoff(SECONDS_PER_DAY - 1, 0), 0);
    }

    // sutra/432: a malformed config fails loudly instead of silently using the
    // default window.
    #[test]
    fn window_days_default_configured_and_malformed() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            window_days(dir.path()).expect("absent config"),
            DEFAULT_WINDOW_DAYS
        );

        let sutra_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&sutra_dir).expect("create .sutra");
        let cfg = sutra_dir.join("components.toml");
        std::fs::write(&cfg, "cochange_window_days = 30\n").expect("write config");
        assert_eq!(window_days(dir.path()).expect("valid config"), 30);

        std::fs::write(&cfg, "cochange_window_days = \"thirty\"\n").expect("write config");
        assert!(window_days(dir.path()).is_err());
    }
}
