//! `sutra firings`: the shared firing log (sutra/467) with an acted-on signal
//! per flagged site, for the sutra/485 re-measure.
//!
//! The signal is whether the flagged line still exists: a site whose line text
//! is gone from the file was changed after the firing. It is a proxy. A line
//! duplicated elsewhere in the file reads as still present, and a change
//! that keeps the line intact (a fix next to it) reads as not acted on.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde_json::json;

use crate::db::Db;
use crate::db::firings::FiringRow;
use crate::error::Result;
use crate::git;

/// What became of a flagged site since it fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteStatus {
    /// The flagged line is still in the file.
    Present,
    /// The flagged line is gone: the site changed.
    Changed,
    /// The file no longer exists.
    FileGone,
    /// No snippet was recorded, so nothing to compare.
    Unknown,
}

impl SiteStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Changed => "changed",
            Self::FileGone => "file_gone",
            Self::Unknown => "unknown",
        }
    }
}

fn site_status(row: &FiringRow, content: Option<&str>) -> SiteStatus {
    let Some(snippet) = row.snippet.as_deref().filter(|s| !s.is_empty()) else {
        return SiteStatus::Unknown;
    };
    match content {
        None => SiteStatus::FileGone,
        Some(text) if text.lines().any(|l| l.trim() == snippet) => SiteStatus::Present,
        Some(_) => SiteStatus::Changed,
    }
}

/// Every firing (optionally one mechanism, fired at or after `since`) with
/// its site status in the current worktree, plus per-mechanism totals.
pub fn handle(
    db: &Db,
    workspace_root: &Path,
    mechanism: Option<&str>,
    since: Option<&str>,
) -> Result<serde_json::Value> {
    let rows = db.firings(mechanism, since)?;
    let mut contents: HashMap<&str, Option<String>> = HashMap::new();
    let mut totals: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let content = match contents.get(row.file_path.as_str()) {
            Some(c) => c,
            None => {
                let c = git::file_content_on_side(workspace_root, None, &row.file_path)?;
                contents.entry(&row.file_path).or_insert(c)
            }
        };
        let status = site_status(row, content.as_deref());
        *totals
            .entry(&row.mechanism)
            .or_default()
            .entry(status.as_str())
            .or_default() += 1;
        out.push(json!({
            "id": row.id,
            "mechanism": row.mechanism,
            "finding_kind": row.finding_kind,
            "finding_key": row.finding_key,
            "file": row.file_path,
            "line": row.line,
            "symbol": row.symbol,
            "snippet": row.snippet,
            "surface": row.surface,
            "diff_spec": row.diff_spec,
            "base_rev": row.base_rev,
            "head_rev": row.head_rev,
            "anchor_commit": row.anchor_commit,
            "diff_fingerprint": row.diff_fingerprint,
            "fired_at": row.fired_at,
            "site_status": status.as_str(),
        }));
    }
    Ok(json!({ "totals": totals, "firings": out }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(snippet: Option<&str>) -> FiringRow {
        FiringRow {
            id: 1,
            mechanism: "sibling_pattern".into(),
            finding_kind: "chain".into(),
            finding_key: "a.b".into(),
            file_path: "src/a.rs".into(),
            line: Some(3),
            symbol: None,
            snippet: snippet.map(str::to_string),
            surface: "check".into(),
            diff_spec: "staged".into(),
            base_rev: None,
            head_rev: None,
            anchor_commit: None,
            diff_fingerprint: "f".into(),
            fired_at: "2026-09-26T00:00:00Z".into(),
        }
    }

    #[test]
    fn status_compares_trimmed_lines() {
        let r = row(Some("a(x).b();"));
        assert_eq!(
            site_status(&r, Some("fn f() {\n    a(x).b();\n}\n")),
            SiteStatus::Present
        );
        assert_eq!(
            site_status(&r, Some("fn f() {\n    a(x)?;\n}\n")),
            SiteStatus::Changed
        );
        assert_eq!(site_status(&r, None), SiteStatus::FileGone);
        assert_eq!(site_status(&row(None), Some("")), SiteStatus::Unknown);
    }
}
