use std::path::Path;

use serde_json::json;

use crate::db::Db;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Fresh,
    Edited,
    Stale,
}

impl FileStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileStatus::Fresh => "fresh",
            FileStatus::Edited => "edited",
            FileStatus::Stale => "stale",
        }
    }
}

pub fn check_file(workspace_root: &Path, relative_path: &str, last_parsed: &str) -> FileStatus {
    let full = workspace_root.join(relative_path);
    let Ok(meta) = std::fs::metadata(&full) else {
        return FileStatus::Stale;
    };
    let Ok(mtime) = meta.modified() else {
        return FileStatus::Stale;
    };
    let Ok(parsed_dt) = chrono::DateTime::parse_from_rfc3339(last_parsed) else {
        return FileStatus::Stale;
    };
    let parsed_sys: std::time::SystemTime = parsed_dt.into();
    if mtime > parsed_sys {
        FileStatus::Edited
    } else {
        FileStatus::Fresh
    }
}

#[derive(Debug, Default)]
pub struct FreshnessCounts {
    pub fresh: usize,
    pub edited: usize,
    pub stale: usize,
}

impl FreshnessCounts {
    pub fn record(&mut self, status: FileStatus) {
        match status {
            FileStatus::Fresh => self.fresh += 1,
            FileStatus::Edited => self.edited += 1,
            FileStatus::Stale => self.stale += 1,
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        json!({
            "fresh": self.fresh,
            "edited": self.edited,
            "stale": self.stale,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SearchTier {
    Exact,
    Fts,
}

impl SearchTier {
    pub fn confidence(&self) -> f64 {
        match self {
            SearchTier::Exact => 1.0,
            SearchTier::Fts => 0.6,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            SearchTier::Exact => "exact",
            SearchTier::Fts => "fts",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessLevel {
    Fresh,
    EditedUncommitted,
    StaleIndex,
}

impl From<FileStatus> for FreshnessLevel {
    fn from(s: FileStatus) -> Self {
        match s {
            FileStatus::Fresh => FreshnessLevel::Fresh,
            FileStatus::Edited => FreshnessLevel::EditedUncommitted,
            FileStatus::Stale => FreshnessLevel::StaleIndex,
        }
    }
}

pub fn confidence_json(tier: SearchTier) -> serde_json::Value {
    json!({
        "score": tier.confidence(),
        "tier": tier.label(),
        "formula": "exact short_name match = 1.0, FTS5 prefix match = 0.6",
    })
}

/// Check whether indexed files have actually changed since last parse.
/// Returns true only if HEAD moved, any indexed file was modified/deleted,
/// or the parse timestamp is unparseable.
pub fn workspace_has_changed(
    workspace_root: &Path,
    parse_timestamp: &str,
    stored_head: Option<&str>,
    indexed_paths: &[String],
) -> bool {
    let current_head = crate::git::head_commit_hash(workspace_root);
    match (stored_head, &current_head) {
        (Some(stored), Some(current)) => {
            if current != stored {
                return true;
            }
        }
        // Git workspace but no stored HEAD (pre-migration snapshot) —
        // force stale so the next parse records a HEAD hash.
        (None, Some(_)) => return true,
        _ => {}
    }

    let Ok(parsed_dt) = chrono::DateTime::parse_from_rfc3339(parse_timestamp) else {
        return true;
    };
    let parsed_sys: std::time::SystemTime = parsed_dt.into();

    for path in indexed_paths {
        let full = workspace_root.join(path);
        match std::fs::metadata(&full) {
            Ok(meta) => {
                if let Ok(mtime) = meta.modified() {
                    if mtime > parsed_sys {
                        return true;
                    }
                }
            }
            Err(_) => return true,
        }
    }

    false
}

pub fn is_workspace_stale(
    db: &Db,
    workspace_root: &Path,
    stale_threshold_sec: u64,
) -> (Option<String>, bool) {
    match db.last_parse_info() {
        Ok(Some((ts, head_commit))) => {
            let is_stale = chrono::DateTime::parse_from_rfc3339(&ts)
                .map(|dt| {
                    let age = chrono::Utc::now() - dt.with_timezone(&chrono::Utc);
                    if age.num_seconds() as u64 <= stale_threshold_sec {
                        return false;
                    }
                    let paths = db.all_indexed_paths().unwrap_or_default();
                    workspace_has_changed(workspace_root, &ts, head_commit.as_deref(), &paths)
                })
                .unwrap_or(true);
            (Some(ts), is_stale)
        }
        _ => (None, true),
    }
}

pub struct FreshnessAnnotator<'a> {
    root: &'a Path,
    counts: FreshnessCounts,
}

impl<'a> FreshnessAnnotator<'a> {
    pub fn new(root: &'a Path) -> Self {
        Self {
            root,
            counts: FreshnessCounts::default(),
        }
    }

    pub fn annotate_file(&mut self, item: &mut serde_json::Value, path: &str, last_parsed: &str) {
        let status = check_file(self.root, path, last_parsed);
        self.counts.record(status);
        item["_freshness"] = json!(status.as_str());
    }

    pub fn counts(&self) -> &FreshnessCounts {
        &self.counts
    }

    pub fn finish(self) -> serde_json::Value {
        self.counts.to_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn fresh_when_file_unmodified_since_parse() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        fs::write(&file, "fn main() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let parsed_at = chrono::Utc::now().to_rfc3339();
        assert_eq!(
            check_file(dir.path(), "test.rs", &parsed_at),
            FileStatus::Fresh
        );
    }

    #[test]
    fn edited_when_file_modified_after_parse() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("test.rs");
        fs::write(&file, "fn main() {}").unwrap();
        let parsed_at = chrono::Utc::now().to_rfc3339();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(&file, "fn main() { changed }").unwrap();
        assert_eq!(
            check_file(dir.path(), "test.rs", &parsed_at),
            FileStatus::Edited
        );
    }

    #[test]
    fn stale_when_file_missing() {
        let dir = tempdir().unwrap();
        let parsed_at = chrono::Utc::now().to_rfc3339();
        assert_eq!(
            check_file(dir.path(), "gone.rs", &parsed_at),
            FileStatus::Stale
        );
    }

    #[test]
    fn counts_aggregate_correctly() {
        let mut counts = FreshnessCounts::default();
        counts.record(FileStatus::Fresh);
        counts.record(FileStatus::Fresh);
        counts.record(FileStatus::Edited);
        counts.record(FileStatus::Stale);
        let j = counts.to_json();
        assert_eq!(j["fresh"], 2);
        assert_eq!(j["edited"], 1);
        assert_eq!(j["stale"], 1);
    }

    #[test]
    fn confidence_exact_is_1() {
        assert_eq!(SearchTier::Exact.confidence(), 1.0);
    }

    #[test]
    fn confidence_fts_is_below_exact() {
        assert!(SearchTier::Fts.confidence() < SearchTier::Exact.confidence());
    }

    #[test]
    fn workspace_unchanged_when_no_files_modified() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "fn hello() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let parsed_at = chrono::Utc::now().to_rfc3339();
        let paths = vec!["lib.rs".to_string()];
        assert!(!workspace_has_changed(dir.path(), &parsed_at, None, &paths));
    }

    #[test]
    fn workspace_changed_when_file_modified() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "fn hello() {}").unwrap();
        let parsed_at = chrono::Utc::now().to_rfc3339();
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(&file, "fn hello() { changed }").unwrap();
        let paths = vec!["lib.rs".to_string()];
        assert!(workspace_has_changed(dir.path(), &parsed_at, None, &paths));
    }

    #[test]
    fn workspace_changed_when_file_deleted() {
        let dir = tempdir().unwrap();
        let parsed_at = chrono::Utc::now().to_rfc3339();
        let paths = vec!["gone.rs".to_string()];
        assert!(workspace_has_changed(dir.path(), &parsed_at, None, &paths));
    }

    #[test]
    fn workspace_not_stale_when_stored_head_but_not_git() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "fn hello() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let parsed_at = chrono::Utc::now().to_rfc3339();
        let paths = vec!["lib.rs".to_string()];
        // Non-git dir: head_commit_hash returns None, stored HEAD present
        // — (Some, None) falls through to mtime check which passes.
        assert!(!workspace_has_changed(
            dir.path(),
            &parsed_at,
            Some("abc123"),
            &paths,
        ));
    }

    #[test]
    fn workspace_not_stale_non_git_no_stored_head() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "fn hello() {}").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let parsed_at = chrono::Utc::now().to_rfc3339();
        let paths = vec!["lib.rs".to_string()];
        // Non-git dir, no stored HEAD — (None, None) falls through to mtime.
        assert!(!workspace_has_changed(dir.path(), &parsed_at, None, &paths,));
    }
}
