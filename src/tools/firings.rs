//! `sutra firings`: the shared firing log (sutra/467) with an acted-on signal
//! per flagged site, for the sutra/485 re-measure. Also the review-event
//! identity every mechanism records under (sutra/491).
//!
//! Event identity. A reviewed change is identified by its [`ReviewedPatch`]:
//! the removed and added lines of each file, without line numbers or context,
//! so reviewing it again, or after a rebase over unrelated context, finds the
//! same event. An event's `epoch` is bumped when history since the last
//! review of the patch holds a commit whose patch is its exact inverse: a
//! revert followed by an identical reapply is a new opportunity. A revert that
//! was never committed is invisible.
//!
//! Acted-on. Evaluated against commits only, never the worktree: from the
//! event's anchor commit (HEAD at firing time) along HEAD's first-parent chain,
//! following renames of the site's file. A site is present while its
//! enclosing symbol still holds at least as many copies of the flagged line as
//! it did at the anchor (the whole file, when the symbol is unknown or gone).
//! The first commit that drops a copy is the site's first change. A rename
//! alone moves the site; it does not change it. The signal is a proxy: a fix
//! that keeps the flagged line intact reads as not acted on, and with several
//! identical lines in one symbol, removing any of them counts.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use serde_json::json;

use crate::db::Db;
use crate::db::firings::{FiringContext, FiringRow, ReviewEvent};
use crate::error::Result;
use crate::git::{self, CommitChanges, FileHunks, WalkFrom};
use crate::parser::adapter::{LanguageRegistry, ParserPool};
use crate::parser::{SymbolSpan, symbol_spans};

// ---------------------------------------------------------------------------
// Review-event identity
// ---------------------------------------------------------------------------

/// Identity of a reviewed change.
#[derive(Debug, Clone, Default)]
pub struct ReviewedPatch {
    /// Hash of the change's removed and added lines, per file.
    pub id: String,
    /// The same hash of the inverse change: what a revert commit would hash to.
    pub inverse: String,
    /// Every path the change touched, either side.
    pub paths: BTreeSet<String>,
}

/// Builds a [`ReviewedPatch`] one file at a time.
#[derive(Default)]
pub struct PatchHasher {
    forward: Vec<[u8; 32]>,
    inverse: Vec<[u8; 32]>,
    paths: BTreeSet<String>,
}

impl PatchHasher {
    /// Add one file of the diff, with its `old` and `new` side source.
    pub fn add_file(&mut self, fh: &FileHunks, old: Option<&str>, new: Option<&str>) {
        let old_lines: Vec<&str> = old.map(|s| s.lines().collect()).unwrap_or_default();
        let new_lines: Vec<&str> = new.map(|s| s.lines().collect()).unwrap_or_default();
        let hunks: Vec<(Vec<&str>, Vec<&str>)> = fh
            .hunks
            .iter()
            .map(|h| {
                (
                    lines_in(&old_lines, h.removed_lines()),
                    lines_in(&new_lines, h.added_lines()),
                )
            })
            .collect();
        let paths = (
            fh.old_path.as_deref().unwrap_or(""),
            fh.new_path.as_deref().unwrap_or(""),
        );
        self.forward.push(file_digest(paths, &hunks, false));
        self.inverse.push(file_digest(paths, &hunks, true));
        self.paths.extend(
            [&fh.old_path, &fh.new_path]
                .into_iter()
                .flatten()
                .map(ToString::to_string),
        );
    }

    pub fn finish(self) -> ReviewedPatch {
        let combine = |mut digests: Vec<[u8; 32]>| {
            digests.sort_unstable();
            let mut h = blake3::Hasher::new();
            for d in &digests {
                h.update(d);
            }
            h.finalize().to_hex().to_string()
        };
        ReviewedPatch {
            id: combine(self.forward),
            inverse: combine(self.inverse),
            paths: self.paths,
        }
    }
}

/// The lines of a 1-based line range.
fn lines_in<'s>(lines: &[&'s str], range: std::ops::Range<usize>) -> Vec<&'s str> {
    range
        .filter_map(|l| lines.get(l.wrapping_sub(1)).copied())
        .collect()
}

/// One file's change: paths, then each hunk's removed and added lines. The
/// inverse swaps the sides, which is exactly what the revert's diff holds.
fn file_digest(
    (old_path, new_path): (&str, &str),
    hunks: &[(Vec<&str>, Vec<&str>)],
    invert: bool,
) -> [u8; 32] {
    let (from, to) = if invert {
        (new_path, old_path)
    } else {
        (old_path, new_path)
    };
    let mut h = blake3::Hasher::new();
    h.update(from.as_bytes());
    h.update(b"\0");
    h.update(to.as_bytes());
    h.update(b"\0");
    for (removed, added) in hunks {
        let (minus, plus) = if invert {
            (added, removed)
        } else {
            (removed, added)
        };
        h.update(b"@");
        for (sign, lines) in [(b"-", minus), (b"+", plus)] {
            for l in lines {
                h.update(sign);
                h.update(l.as_bytes());
                h.update(b"\n");
            }
        }
    }
    h.finalize().into()
}

/// The review event `patch` belongs to: the latest one for the same patch,
/// unless a revert of it was committed since, in which case a new epoch.
pub fn resolve_event(
    db: &Db,
    workspace_root: &Path,
    ctx: &FiringContext<'_>,
    patch: &ReviewedPatch,
) -> Result<i64> {
    let epoch = match db.latest_review_event(&patch.id)? {
        None => 0,
        Some(prev) if reverted_since(workspace_root, &prev, patch)? => prev.epoch + 1,
        Some(prev) => return Ok(prev.id),
    };
    db.ensure_review_event(ctx, &patch.id, epoch)
}

/// Whether a commit after `prev` fired reverts `patch`. The window starts at
/// `prev`'s anchor, or at its firing time when the anchor was rewritten away.
fn reverted_since(
    workspace_root: &Path,
    prev: &ReviewEvent,
    patch: &ReviewedPatch,
) -> Result<bool> {
    let Some(head) = git::head_commit_hash(workspace_root) else {
        return Ok(false);
    };
    let from = match prev.anchor_commit.as_deref() {
        Some(a) if git::is_ancestor(workspace_root, a, &head)? => WalkFrom::After(a),
        _ => WalkFrom::Since(&prev.fired_at),
    };
    for commit in git::git_first_parent_changes(workspace_root, from, &head)? {
        let touches = commit.entries.iter().any(|e| {
            patch.paths.contains(&e.path)
                || e.old_path.as_ref().is_some_and(|p| patch.paths.contains(p))
        });
        if touches
            && commit_patch(workspace_root, &commit, &patch.paths)?
                .is_some_and(|id| id == patch.inverse)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The patch id of `commit` against its first parent, restricted to `paths`.
/// `None` for a root commit.
fn commit_patch(
    workspace_root: &Path,
    commit: &CommitChanges,
    paths: &BTreeSet<String>,
) -> Result<Option<String>> {
    let Some(parent) = commit.first_parent.as_deref() else {
        return Ok(None);
    };
    let mut hasher = PatchHasher::default();
    for fh in git::git_diff_hunks(workspace_root, parent, Some(&commit.hash))? {
        let in_scope = [&fh.old_path, &fh.new_path]
            .into_iter()
            .flatten()
            .any(|p| paths.contains(p));
        if !in_scope {
            continue;
        }
        let old = match fh.old_path.as_deref() {
            Some(p) => git::git_file_content_at(workspace_root, parent, p)?,
            None => None,
        };
        let new = match fh.new_path.as_deref() {
            Some(p) => git::git_file_content_at(workspace_root, &commit.hash, p)?,
            None => None,
        };
        hasher.add_file(&fh, old.as_deref(), new.as_deref());
    }
    Ok(Some(hasher.finish().id))
}

// ---------------------------------------------------------------------------
// Acted-on evaluation
// ---------------------------------------------------------------------------

/// What became of a flagged site between its anchor commit and HEAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteStatus {
    /// The flagged line is still there (possibly in a renamed file).
    Present,
    /// A commit removed the flagged line: acted on.
    Changed,
    /// A commit deleted the file (not a rename). Not a site change by itself.
    FileDeleted,
    /// The flagged line was not in the anchor commit (it existed only in the
    /// index or worktree when it fired), so there is nothing to measure from.
    NotAtAnchor,
    /// The anchor is not an ancestor of HEAD (history was rewritten).
    AnchorUnreachable,
    /// No snippet or no anchor was recorded, or there is no HEAD.
    Unknown,
}

impl SiteStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::Changed => "changed",
            Self::FileDeleted => "file_deleted",
            Self::NotAtAnchor => "not_at_anchor",
            Self::AnchorUnreachable => "anchor_unreachable",
            Self::Unknown => "unknown",
        }
    }
}

/// A site's status, where its file lives at HEAD, and the commits that moved
/// it and first changed it, kept apart so a rename never reads as a change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteOutcome<'a> {
    pub status: SiteStatus,
    /// The file's path at the last commit walked.
    pub path: &'a str,
    /// The first commit that renamed the file.
    pub moved_in: Option<&'a str>,
    /// The commit that removed the flagged line, or deleted the file.
    pub changed_in: Option<&'a str>,
}

/// How many lines in `range` (1-based line numbers) of `text` read `snippet`
/// once trimmed.
pub fn count_snippet(text: &str, range: std::ops::Range<usize>, snippet: &str) -> usize {
    text.lines()
        .enumerate()
        .filter(|(i, l)| range.contains(&(i + 1)) && l.trim() == snippet)
        .count()
}

/// Copies of a snippet in one file: inside its symbol, and anywhere.
#[derive(Debug, Clone, Copy)]
struct Copies {
    in_symbol: Option<usize>,
    in_file: usize,
}

impl Copies {
    /// Count `snippet` in `text`, and inside `symbol` when it parses there. A
    /// parse failure leaves only the whole-file count, the documented fallback.
    fn count(
        (pool, registry): (&mut ParserPool, &LanguageRegistry),
        path: &str,
        text: &str,
        symbol: Option<&str>,
        snippet: &str,
    ) -> Self {
        let in_file = count_snippet(text, 1..usize::MAX, snippet);
        let in_symbol = symbol.and_then(|name| {
            let ext = Path::new(path).extension()?.to_str()?;
            let adapter = registry.adapter_for_extension(ext)?;
            let spans: Vec<SymbolSpan> = symbol_spans(pool, adapter, text, path).ok()?;
            let (start, end, _) = spans.into_iter().find(|(_, _, n)| n == name)?;
            Some(count_snippet(text, start..end + 1, snippet))
        });
        Self { in_symbol, in_file }
    }

    /// Whether no copy present at `anchor` has gone.
    fn keeps(self, anchor: Copies) -> bool {
        match (anchor.in_symbol, self.in_symbol) {
            (Some(was), Some(now)) => now >= was,
            _ => self.in_file >= anchor.in_file,
        }
    }
}

type Contents = HashMap<(String, String), Option<String>>;

/// `path` at `commit`, read once.
fn content_at<'c>(
    contents: &'c mut Contents,
    root: &Path,
    commit: &str,
    path: &str,
) -> Result<Option<&'c str>> {
    let slot = match contents.entry((commit.to_string(), path.to_string())) {
        Entry::Occupied(e) => e.into_mut(),
        Entry::Vacant(e) => e.insert(git::git_file_content_at(root, commit, path)?),
    };
    Ok(slot.as_deref())
}

struct Evaluator<'a> {
    root: &'a Path,
    registry: &'a LanguageRegistry,
    pool: ParserPool,
    /// Commits from each anchor to HEAD; `None` when the anchor is not an
    /// ancestor.
    walks: HashMap<String, Option<Vec<CommitChanges>>>,
    contents: Contents,
}

impl Evaluator<'_> {
    fn site<'s>(&'s mut self, row: &'s FiringRow, head: Option<&str>) -> Result<SiteOutcome<'s>> {
        let Evaluator {
            root,
            registry,
            pool,
            walks,
            contents,
        } = self;
        let mut out = SiteOutcome {
            status: SiteStatus::Unknown,
            path: &row.file_path,
            moved_in: None,
            changed_in: None,
        };
        let snippet = row.snippet.as_deref().filter(|s| !s.is_empty());
        let (Some(snippet), Some(anchor), Some(head)) =
            (snippet, row.anchor_commit.as_deref(), head)
        else {
            return Ok(out);
        };
        let symbol = row.symbol.as_deref();
        let Some(at_anchor) = content_at(contents, root, anchor, &row.file_path)? else {
            out.status = SiteStatus::NotAtAnchor;
            return Ok(out);
        };
        let base = Copies::count((pool, registry), &row.file_path, at_anchor, symbol, snippet);
        if base.in_file == 0 {
            out.status = SiteStatus::NotAtAnchor;
            return Ok(out);
        }
        if !walks.contains_key(anchor) {
            let commits = if git::is_ancestor(root, anchor, head)? {
                Some(git::git_first_parent_changes(
                    root,
                    WalkFrom::After(anchor),
                    head,
                )?)
            } else {
                None
            };
            walks.insert(anchor.to_string(), commits);
        }
        let walks: &'s HashMap<String, Option<Vec<CommitChanges>>> = walks;
        let Some(commits) = walks.get(anchor).and_then(Option::as_deref) else {
            out.status = SiteStatus::AnchorUnreachable;
            return Ok(out);
        };
        for c in commits {
            let Some(e) = c.entries.iter().find(|e| e.base_path() == out.path) else {
                continue;
            };
            if e.old_path.is_some() && out.moved_in.is_none() {
                out.moved_in = Some(&c.hash);
            }
            out.path = &e.path;
            let Some(text) = content_at(contents, root, &c.hash, out.path)? else {
                out.status = SiteStatus::FileDeleted;
                out.changed_in = Some(&c.hash);
                return Ok(out);
            };
            if !Copies::count((pool, registry), out.path, text, symbol, snippet).keeps(base) {
                out.status = SiteStatus::Changed;
                out.changed_in = Some(&c.hash);
                return Ok(out);
            }
        }
        out.status = SiteStatus::Present;
        Ok(out)
    }
}

/// Every firing (optionally one mechanism, fired at or after `since`) with
/// its site status at HEAD, plus per-mechanism totals.
pub fn handle(
    db: &Db,
    workspace_root: &Path,
    registry: &LanguageRegistry,
    mechanism: Option<&str>,
    since: Option<&str>,
) -> Result<serde_json::Value> {
    let rows = db.firings(mechanism, since)?;
    let head = git::head_commit_hash(workspace_root);
    let mut eval = Evaluator {
        root: workspace_root,
        registry,
        pool: ParserPool::new(std::time::Duration::from_secs(5)),
        walks: HashMap::new(),
        contents: HashMap::new(),
    };
    let mut totals: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let site = eval.site(row, head.as_deref())?;
        *totals
            .entry(&row.mechanism)
            .or_default()
            .entry(site.status.as_str())
            .or_default() += 1;
        out.push(json!({
            "id": row.id,
            "event_id": row.event_id,
            "mechanism": row.mechanism,
            "finding_kind": row.finding_kind,
            "finding_key": row.finding_key,
            "file": row.file_path,
            "line": row.line,
            "symbol": row.symbol,
            "snippet": row.snippet,
            "occurrence": row.occurrence,
            "surface": row.surface,
            "diff_spec": row.diff_spec,
            "base_rev": row.base_rev,
            "head_rev": row.head_rev,
            "anchor_commit": row.anchor_commit,
            "patch_id": row.patch_id,
            "epoch": row.epoch,
            "fired_at": row.fired_at,
            "site_status": site.status.as_str(),
            "file_at_head": site.path,
            "moved_in": site.moved_in,
            "changed_in": site.changed_in,
        }));
    }
    Ok(json!({ "totals": totals, "firings": out }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Hunk;

    fn fh(old: Option<&str>, new: Option<&str>, hunks: Vec<Hunk>) -> FileHunks {
        FileHunks {
            old_path: old.map(str::to_string),
            new_path: new.map(str::to_string),
            hunks,
        }
    }

    fn hunk(old_start: usize, old_len: usize, new_start: usize, new_len: usize) -> Hunk {
        Hunk {
            old_start,
            old_len,
            new_start,
            new_len,
        }
    }

    fn patch(fh: &FileHunks, old: &str, new: &str) -> ReviewedPatch {
        let mut h = PatchHasher::default();
        h.add_file(fh, Some(old), Some(new));
        h.finish()
    }

    #[test]
    fn patch_id_ignores_line_numbers_and_context() {
        let a = patch(
            &fh(Some("a.rs"), Some("a.rs"), vec![hunk(2, 1, 2, 1)]),
            "x\nold\ny\n",
            "x\nnew\ny\n",
        );
        let shifted = patch(
            &fh(Some("a.rs"), Some("a.rs"), vec![hunk(4, 1, 4, 1)]),
            "p\nq\nr\nold\nz\n",
            "p\nq\nr\nnew\nz\n",
        );
        assert_eq!(a.id, shifted.id);
        assert_ne!(a.id, a.inverse);
    }

    #[test]
    fn inverse_is_the_revert_patch() {
        let forward = patch(
            &fh(Some("a.rs"), Some("b.rs"), vec![hunk(2, 1, 2, 1)]),
            "x\nold\n",
            "x\nnew\n",
        );
        let revert = patch(
            &fh(Some("b.rs"), Some("a.rs"), vec![hunk(2, 1, 2, 1)]),
            "x\nnew\n",
            "x\nold\n",
        );
        assert_eq!(forward.inverse, revert.id);
        assert_eq!(revert.inverse, forward.id);
    }

    #[test]
    fn copies_prefer_the_symbol_scope() {
        let at = Copies {
            in_symbol: Some(1),
            in_file: 2,
        };
        let other_copy_gone = Copies {
            in_symbol: Some(1),
            in_file: 1,
        };
        assert!(other_copy_gone.keeps(at));
        let symbol_gone = Copies {
            in_symbol: None,
            in_file: 2,
        };
        assert!(symbol_gone.keeps(at));
        let this_copy_gone = Copies {
            in_symbol: Some(0),
            in_file: 1,
        };
        assert!(!this_copy_gone.keeps(at));
    }

    #[test]
    fn count_snippet_is_one_based_and_trimmed() {
        let text = "a();\n  a();\nb();\n";
        assert_eq!(count_snippet(text, 1..usize::MAX, "a();"), 2);
        assert_eq!(count_snippet(text, 2..3, "a();"), 1);
        assert_eq!(count_snippet(text, 1..2, "a();"), 1);
    }
}
