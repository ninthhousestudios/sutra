//! Shared plumbing for the review-time advisories that read the index as the
//! reviewed side of a diff (orphans, sutra/483; dup-exists, sutra/469).
//!
//! The index holds the worktree. A diff that ends at the worktree, the staged
//! index or HEAD is close enough, file by file; a historical commit is not,
//! anywhere in the tree. Where the index cannot stand for the reviewed side
//! these helpers say why, so a check reports `incomplete` or `skipped`, never
//! clean.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use crate::db::Db;
use crate::error::Result;
use crate::freshness::{self, FileStatus};
use crate::git;
use crate::parser::adapter::{LanguageAdapter, LanguageRegistry};
use crate::tools::review::DiffScope;

/// Why the index cannot hold the reviewed side of `scope` at all: the diff
/// ends at a commit other than HEAD.
pub(crate) fn index_cannot_hold(workspace_root: &Path, scope: &DiffScope) -> Option<String> {
    let rev = scope.head_revision.as_deref().filter(|r| !r.is_empty())?;
    (git::head_commit_hash(workspace_root).as_deref() != Some(rev)).then(|| {
        format!("the index holds the worktree, not {rev}; review a diff that ends at HEAD")
    })
}

pub(crate) fn adapter_for<'r>(
    registry: &'r LanguageRegistry,
    path: &str,
) -> Option<&'r dyn LanguageAdapter> {
    let ext = Path::new(path).extension()?.to_str()?;
    registry.adapter_for_extension(ext)
}

/// Why the index cannot stand for the reviewed side of `path`, if it cannot:
/// indexed before its last edit, or the reviewed content (a commit, the
/// staged index) differs from the worktree the index holds.
pub(crate) fn index_mismatch(
    db: &Db,
    workspace_root: &Path,
    scope: &DiffScope,
    path: &str,
) -> Option<String> {
    let status = match db.file_by_path(path) {
        Ok(Some(f)) => freshness::check_file(workspace_root, path, &f.last_parsed),
        Ok(None) => return Some(format!("{path}: not in the index")),
        Err(e) => return Some(format!("{path}: {e}")),
    };
    if !matches!(status, FileStatus::Fresh) {
        return Some(format!("{path}: edited since it was indexed"));
    }
    let head = scope.head_revision.as_deref()?;
    let reviewed = git::file_content_on_side(workspace_root, Some(head), path);
    let worktree = git::file_content_on_side(workspace_root, None, path);
    match (reviewed, worktree) {
        (Ok(a), Ok(b)) if a == b => None,
        (Ok(_), Ok(_)) => Some(format!(
            "{path}: the worktree differs from the reviewed side"
        )),
        (Err(e), _) | (_, Err(e)) => Some(format!("{path}: {e}")),
    }
}

/// Why liveness read from the index may not be the reviewed side's, outside
/// the diff: `index_mismatch` covers the changed files, but the index holds
/// the whole worktree, so an uncommitted caller elsewhere hides an orphan and
/// a deleted one invents one. Only a staged or HEAD review can differ; an
/// unstaged review is the worktree.
pub(crate) fn dirty_outside_diff(
    db: &Db,
    workspace_root: &Path,
    scope: &DiffScope,
    registry: &LanguageRegistry,
) -> Result<Option<String>> {
    let Some(head) = scope.head_revision.as_deref() else {
        return Ok(None);
    };
    let in_diff: HashSet<&str> = scope
        .entries
        .iter()
        .flat_map(|e| [e.path.as_str(), e.base_path()])
        .collect();
    let changed = git::git_diff_entries_to_worktree(workspace_root, head)?;
    let untracked = git::untracked_files(workspace_root)?;
    let mut dirty = BTreeSet::new();
    for path in changed
        .iter()
        .flat_map(|e| [e.path.as_str(), e.base_path()])
        .chain(untracked.iter().map(String::as_str))
    {
        if in_diff.contains(path) || dirty.contains(path) {
            continue;
        }
        if let Some(adapter) = adapter_for(registry, path)
            && db.indexes_language(adapter.language_id())?
        {
            dirty.insert(path);
        }
    }
    if dirty.is_empty() {
        return Ok(None);
    }
    const SHOWN: usize = 5;
    let mut listed: Vec<&str> = dirty.iter().copied().take(SHOWN).collect();
    let more = dirty.len().saturating_sub(SHOWN);
    let tail = format!("(+{more} more)");
    if more > 0 {
        listed.push(&tail);
    }
    Ok(Some(format!(
        "{} indexed file(s) outside the diff differ in the worktree from the reviewed side: {}",
        dirty.len(),
        listed.join(", ")
    )))
}

/// The trimmed text of each `(file, line)` on the reviewed side, for the
/// firing log's site identity.
pub(crate) fn line_snippets<'a>(
    workspace_root: &Path,
    scope: &DiffScope,
    sites: impl Iterator<Item = (&'a str, i64)>,
) -> Result<Vec<String>> {
    let mut texts: BTreeMap<&str, String> = BTreeMap::new();
    let mut snippets = Vec::new();
    for (file, line) in sites {
        if !texts.contains_key(file) {
            let text =
                git::file_content_on_side(workspace_root, scope.head_revision.as_deref(), file)?
                    .unwrap_or_default();
            texts.insert(file, text);
        }
        let line = usize::try_from(line).expect("invariant: a source line number is positive");
        let snippet = texts[file]
            .lines()
            .nth(line.saturating_sub(1))
            .unwrap_or("")
            .trim()
            .to_string();
        snippets.push(snippet);
    }
    Ok(snippets)
}
