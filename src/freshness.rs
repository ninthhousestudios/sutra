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

/// Byte-level drift of the workspace against its indexed baseline: source files
/// that changed content, appeared, or vanished since the last parse. Structured
/// (not a bool) so a reparse can touch only what moved (sutra/362).
#[derive(Debug, Default, Clone)]
pub struct WorkspaceDrift {
    /// Indexed files whose bytes differ from the stored `content_hash`.
    pub changed: Vec<String>,
    /// Source files present on disk with no indexed row.
    pub added: Vec<String>,
    /// Indexed files the workspace walk no longer yields (deleted or now ignored).
    pub removed: Vec<String>,
}

impl WorkspaceDrift {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.added.is_empty() && self.removed.is_empty()
    }
}

/// True when the file on disk still matches its stored fingerprint. Fast path:
/// if `(size, mtime)` both match the baseline, the file is clean without a read.
/// On any mismatch — or a missing baseline (pre-migration rows) — confirm by
/// hashing the bytes, so a `touch` that preserves content reads as clean and a
/// mtime-preserving edit is still caught.
fn file_matches_fingerprint(full: &Path, fp: &crate::db::FileFingerprint) -> bool {
    let Ok(meta) = std::fs::metadata(full) else {
        // Cannot stat (vanished mid-walk / permission) — the reparse handles it.
        return false;
    };
    let size = meta.len() as i64;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64);
    if fp.size_bytes == Some(size) && mtime_ns.is_some() && fp.mtime_ns == mtime_ns {
        return true;
    }
    match std::fs::read(full) {
        Ok(bytes) => blake3::hash(&bytes).to_hex().to_string() == fp.content_hash,
        Err(_) => false,
    }
}

/// Compute drift by walking the workspace with the same walker/ignore rules the
/// parse pipeline uses (so newly created source files are visible) and comparing
/// each source file to its stored fingerprint. `allowed_extensions` scopes the
/// walk to indexable files, so a docs-only change never registers as drift.
pub fn probe_drift(
    workspace_root: &Path,
    stored: &[crate::db::FileFingerprint],
    allowed_extensions: &[&str],
) -> WorkspaceDrift {
    use std::collections::HashMap;

    // Drain the index as we walk: a matched path is removed, so whatever remains
    // afterward is exactly the set the walk never yielded — the removed files.
    let mut index: HashMap<&str, &crate::db::FileFingerprint> =
        stored.iter().map(|fp| (fp.path.as_str(), fp)).collect();
    let mut drift = WorkspaceDrift::default();

    for full in crate::pipeline::walk_source_files(workspace_root, allowed_extensions) {
        let rel = full
            .strip_prefix(workspace_root)
            .unwrap_or(&full)
            .to_string_lossy()
            .to_string();
        match index.remove(rel.as_str()) {
            None => drift.added.push(rel),
            Some(fp) if !file_matches_fingerprint(&full, fp) => drift.changed.push(rel),
            Some(_) => {} // clean
        }
    }

    drift.removed = index.into_keys().map(str::to_string).collect();
    drift
}

/// Drift of the workspace against the last parse. Returns
/// `(last_parse_timestamp, drift)`; `drift` is `None` when there is no baseline
/// to compare against (no prior parse, or the fingerprint read failed) — the
/// caller should treat that as "must parse", never as "clean".
pub fn workspace_drift(
    db: &Db,
    workspace_root: &Path,
    languages: &[String],
) -> (Option<String>, Option<WorkspaceDrift>) {
    let ts = match db.last_parse_info() {
        Ok(Some((ts, _head))) => ts,
        _ => return (None, None),
    };
    // Distinguish "no files" from "failed to look": a read error must not read as
    // an empty baseline (which would flag every file removed).
    let Ok(stored) = db.all_file_fingerprints() else {
        return (Some(ts), None);
    };
    let registry = crate::parser::adapter::default_registry();
    let allowed_extensions = registry.extensions_for_languages(languages);
    let drift = probe_drift(workspace_root, &stored, &allowed_extensions);
    (Some(ts), Some(drift))
}

/// Whether the index no longer reflects the workspace bytes. Staleness is a claim
/// about content, never about elapsed time — there is no grace window. Git HEAD
/// is not consulted: a docs-only commit (HEAD moves, no indexed file changes)
/// must not invalidate the index (sutra/319, sutra/362).
pub fn is_workspace_stale(
    db: &Db,
    workspace_root: &Path,
    languages: &[String],
) -> (Option<String>, bool) {
    match workspace_drift(db, workspace_root, languages) {
        (ts, Some(drift)) => (ts, !drift.is_empty()),
        // No baseline / read failure — must parse.
        (ts, None) => (ts, true),
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

    use crate::db::FileFingerprint;

    const RS: &[&str] = &["rs"];

    /// Fingerprint a file exactly as a parse would: size, mtime, and blake3 of
    /// its bytes captured from what is currently on disk.
    fn fingerprint_of(root: &Path, rel: &str) -> FileFingerprint {
        let full = root.join(rel);
        let meta = fs::metadata(&full).unwrap();
        let mtime_ns = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i64;
        let bytes = fs::read(&full).unwrap();
        FileFingerprint {
            path: rel.to_string(),
            size_bytes: Some(meta.len() as i64),
            mtime_ns: Some(mtime_ns),
            content_hash: blake3::hash(&bytes).to_hex().to_string(),
        }
    }

    #[test]
    fn clean_when_no_file_modified() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn hello() {}").unwrap();
        let stored = vec![fingerprint_of(dir.path(), "lib.rs")];
        assert!(probe_drift(dir.path(), &stored, RS).is_empty());
    }

    #[test]
    fn edit_detected_immediately_without_grace_window() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "fn hello() {}").unwrap();
        let stored = vec![fingerprint_of(dir.path(), "lib.rs")];
        // Edit and probe in the same instant — the old grace window would have
        // reported this clean; content-based staleness must catch it now.
        fs::write(&file, "fn hello() { changed }").unwrap();
        let drift = probe_drift(dir.path(), &stored, RS);
        assert_eq!(drift.changed, vec!["lib.rs".to_string()]);
        assert!(drift.added.is_empty() && drift.removed.is_empty());
    }

    #[test]
    fn new_source_file_is_added_drift() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn hello() {}").unwrap();
        let stored = vec![fingerprint_of(dir.path(), "lib.rs")];
        // A brand-new source file with no indexed row (no commit needed).
        fs::write(dir.path().join("new.rs"), "fn other() {}").unwrap();
        let drift = probe_drift(dir.path(), &stored, RS);
        assert_eq!(drift.added, vec!["new.rs".to_string()]);
        assert!(drift.changed.is_empty() && drift.removed.is_empty());
    }

    #[test]
    fn touch_without_content_change_is_clean() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "fn hello() {}").unwrap();
        let stored = vec![fingerprint_of(dir.path(), "lib.rs")];
        // Rewrite identical bytes after a delay: mtime moves, size and content
        // do not. The (size, mtime) fast path misses, the hash confirm rescues.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&file, "fn hello() {}").unwrap();
        assert!(probe_drift(dir.path(), &stored, RS).is_empty());
    }

    #[test]
    fn docs_only_change_is_not_drift() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("lib.rs"), "fn hello() {}").unwrap();
        fs::write(dir.path().join("README.md"), "# hi").unwrap();
        let stored = vec![fingerprint_of(dir.path(), "lib.rs")];
        // Modify and add non-source files — outside `allowed_extensions`, so the
        // walk never sees them and they produce no drift.
        fs::write(dir.path().join("README.md"), "# changed").unwrap();
        fs::write(dir.path().join("NOTES.md"), "new").unwrap();
        assert!(probe_drift(dir.path(), &stored, RS).is_empty());
    }

    #[test]
    fn deleted_file_is_removed_drift() {
        let dir = tempdir().unwrap();
        // Stored baseline references a file that is not on disk.
        let stored = vec![FileFingerprint {
            path: "gone.rs".to_string(),
            size_bytes: Some(10),
            mtime_ns: Some(1),
            content_hash: "deadbeef".to_string(),
        }];
        let drift = probe_drift(dir.path(), &stored, RS);
        assert_eq!(drift.removed, vec!["gone.rs".to_string()]);
        assert!(drift.changed.is_empty() && drift.added.is_empty());
    }

    /// Not a correctness test — a manual probe-cost measurement on the sutra
    /// repo itself (sutra/362 acceptance). Run: `cargo test --lib
    /// measure_probe_cost -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn measure_probe_cost_on_self() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        // Build an all-clean baseline exactly as a parse would (setup, untimed).
        let stored: Vec<FileFingerprint> = crate::pipeline::walk_source_files(root, RS)
            .iter()
            .filter_map(|full| {
                let rel = full.strip_prefix(root).ok()?.to_string_lossy().to_string();
                Some(fingerprint_of(root, &rel))
            })
            .collect();
        // Warm the page cache, then take the best of several runs.
        let _ = probe_drift(root, &stored, RS);
        let mut best = std::time::Duration::MAX;
        for _ in 0..5 {
            let t = std::time::Instant::now();
            let drift = probe_drift(root, &stored, RS);
            best = best.min(t.elapsed());
            assert!(drift.is_empty(), "self should be clean: {drift:?}");
        }
        println!(
            "probe_drift over {} files: {:?} warm (best of 5)",
            stored.len(),
            best
        );
    }

    #[test]
    fn missing_baseline_falls_through_to_hash_confirm() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        fs::write(&file, "fn hello() {}").unwrap();
        // Pre-migration row: no size/mtime baseline, only content_hash. Fast path
        // cannot fire; the hash confirm must still recognize the file as clean.
        let bytes = fs::read(&file).unwrap();
        let stored = vec![FileFingerprint {
            path: "lib.rs".to_string(),
            size_bytes: None,
            mtime_ns: None,
            content_hash: blake3::hash(&bytes).to_hex().to_string(),
        }];
        assert!(probe_drift(dir.path(), &stored, RS).is_empty());
    }
}
