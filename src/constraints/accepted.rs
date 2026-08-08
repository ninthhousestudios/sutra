//! Portable, version-controllable constraint waivers and instance acks.
//!
//! sutra/303. Waivers (`constraint_waivers`) and instance acks
//! (`constraint_instance_acks`, sutra/305) previously lived only in the
//! per-workspace SQLite DB under `.sutra/<workspace>/`, which is gitignored — so
//! they never travelled: a fresh clone, CI, or a teammate lost every accepted
//! decision and its rationale, and the review report re-flagged everything.
//!
//! This module makes `.sutra/accepted.toml` the **single source of truth**. The
//! two DB tables are demoted to a projection rebuilt from the file (one writer
//! direction, file -> DB, so no bidirectional drift is possible). The existing
//! DB read paths — the guard's `find_waiver`, the report's waiver partition, the
//! `evaluate_dd` ack subtraction — are left untouched; a freshness gate in front
//! of them re-projects the cache when the file changes, so guard and report keep
//! deriving acceptance through the *same* table (guard must predict the audit).
//!
//! Two decisions shape the schema:
//! - **Keyed by constraint NAME, not the blake3 id.** The id is
//!   `blake3(kind, params, scope)`; a scope/param edit reshapes it and would
//!   orphan every entry keyed to it. In a *tracked* file that means a confusing
//!   diff and lost rationale across the whole team. Names survive such edits and
//!   read cleanly in review. Names are resolved to a live `Constraint` at load;
//!   unknown/ambiguous names are surfaced, never silently dropped (a dropped
//!   waiver re-flags reviewed code; a dropped ack re-surfaces noise).
//! - **No timestamps in the file.** git blame is the audit trail; `created_at`
//!   in a tracked file only produces merge churn. The cache synthesizes them.

use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::db::{AckProjection, Db, WaiverProjection};
use crate::error::{Result, SutraError};
use crate::rules::Constraint;

/// Location of the tracked file, relative to the workspace root.
pub fn accepted_path(root: &Path) -> PathBuf {
    root.join(".sutra").join("accepted.toml")
}

// ---------------------------------------------------------------------------
// File schema (`.sutra/accepted.toml`)
// ---------------------------------------------------------------------------

/// The whole tracked file. Two arrays with distinct semantics deliberately share
/// one file so a waiver and its sibling acks review side-by-side.
#[derive(Debug, Default, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AcceptedFile {
    /// Guard-honored, coarse suppression (also removes the finding from reports).
    #[serde(default, rename = "waiver", skip_serializing_if = "Vec::is_empty")]
    pub waivers: Vec<WaiverEntry>,
    /// Report-only, content-keyed, count-aware acknowledgments (sutra/305). Never
    /// consulted by the guard — future/surplus clones stay governed.
    #[serde(default, rename = "ack", skip_serializing_if = "Vec::is_empty")]
    pub acks: Vec<AckEntry>,
}

/// A guard-honored waiver, scoped to `(constraint, file[, symbol])`.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct WaiverEntry {
    /// Constraint NAME (human-stable; resolved to an id at load).
    pub constraint: String,
    pub file: String,
    /// `symbol_qualified_name`; absent = whole-file waiver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// Why this is acceptable. Required — the stranded rationale is the whole point.
    pub rationale: String,
    pub by: String,
}

/// A report-only instance ack. Content-keyed by the guard's `MatchKey` parts
/// (`enclosing_symbol`, `snippet`) plus `count`; surplus/future siblings surface.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AckEntry {
    /// Constraint NAME (human-stable; resolved to an id at load).
    pub constraint: String,
    pub file: String,
    /// `enclosing_symbol` — the guard `MatchKey` part.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// `snippet` — the matched node's first line, node-relative. The guard
    /// `MatchKey` part; reindent-stable, rename-fragile (a renamed clone
    /// re-surfaces, which is correct — re-examine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// `accepted_count`: how many matches of this key are examined-and-accepted.
    #[serde(default = "one")]
    pub count: u32,
    /// Optional for a bulk `baseline`; required for a single `ack`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    pub by: String,
}

fn one() -> u32 {
    1
}

// ---------------------------------------------------------------------------
// Name resolution
// ---------------------------------------------------------------------------

/// Outcome of resolving a file entry's `constraint` name against the live rule
/// set. Ambiguity and absence are distinct, reportable states — never folded
/// into "no match" (a silently dropped waiver re-flags reviewed code).
#[derive(Debug)]
pub enum RefResolution<'a> {
    Resolved(&'a Constraint),
    /// Name matches no live constraint (rule removed, renamed, or typo).
    Unknown,
    /// Name matches more than one live constraint. Parse-time uniqueness
    /// enforcement (rules.rs) makes this unreachable for a well-formed rule set,
    /// but it is honored defensively rather than picking an arbitrary match.
    Ambiguous(usize),
}

/// Resolve a constraint name to exactly one live `Constraint`.
pub fn resolve_ref<'a>(name: &str, constraints: &'a [Constraint]) -> RefResolution<'a> {
    let mut matches = constraints
        .iter()
        .filter(|c| c.name.as_deref() == Some(name));
    match matches.next() {
        None => RefResolution::Unknown,
        Some(first) => {
            let extra = matches.count();
            if extra == 0 {
                RefResolution::Resolved(first)
            } else {
                RefResolution::Ambiguous(extra + 1)
            }
        }
    }
}

/// Why a file entry could not be projected into the cache. Collected and
/// surfaced (on load, and on the report), not raised as a hard error — one bad
/// row must not blind the rest of the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedWarning {
    pub kind: AcceptedWarningKind,
    /// The unresolved `constraint` field, verbatim, for the operator to fix.
    pub constraint_ref: String,
    pub file: String,
}

impl AcceptedWarning {
    /// One-line, operator-facing description for the report surface.
    pub fn message(&self) -> String {
        match self.kind {
            AcceptedWarningKind::UnknownConstraint => format!(
                "accepted.toml: entry for `{}` ({}) names no live constraint — it \
                 suppresses nothing and its rationale is stranded",
                self.constraint_ref, self.file
            ),
            AcceptedWarningKind::AmbiguousConstraint => format!(
                "accepted.toml: entry for `{}` ({}) matches multiple constraints — \
                 give the constraints unique names so it can be honored",
                self.constraint_ref, self.file
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptedWarningKind {
    UnknownConstraint,
    AmbiguousConstraint,
}

// ---------------------------------------------------------------------------
// Load / resolve
// ---------------------------------------------------------------------------

/// Result of resolving the file against the live rule set: entries that resolved,
/// ready to write to the cache, plus warnings for those that did not.
#[derive(Debug, Default)]
pub struct AcceptedLoad {
    pub waivers: Vec<WaiverProjection>,
    pub acks: Vec<AckProjection>,
    pub warnings: Vec<AcceptedWarning>,
}

/// Read + parse `.sutra/accepted.toml`. Absent file -> empty (a repo may have
/// none). IO or parse failure -> hard `Err`: a malformed file must fail loud,
/// never silently read as "no acceptances" (that re-flags reviewed code).
pub fn load_accepted_file(root: &Path) -> Result<AcceptedFile> {
    Ok(read_file_and_hash(root)?.0)
}

/// Read the raw file once and return both the parsed model and a content hash of
/// the bytes on disk (the freshness marker). Absent file hashes as empty bytes so
/// "no file" and "empty cache" agree.
fn read_file_and_hash(root: &Path) -> Result<(AcceptedFile, String)> {
    let path = accepted_path(root);
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            return Err(SutraError::Internal(format!(
                "reading {}: {e}",
                path.display()
            )));
        }
    };
    let hash = blake3::hash(content.as_bytes()).to_hex().to_string();
    let file = if content.trim().is_empty() {
        AcceptedFile::default()
    } else {
        toml::from_str(&content)
            .map_err(|e| SutraError::Internal(format!("{}: parse error: {e}", path.display())))?
    };
    Ok((file, hash))
}

/// Resolve every entry against the live constraints, partitioning into
/// projectable rows and warnings. Consumes the file: entry fields are *moved*
/// into the owned projections rather than cloned.
pub fn resolve_accepted(file: AcceptedFile, constraints: &[Constraint]) -> AcceptedLoad {
    let mut load = AcceptedLoad::default();
    for w in file.waivers {
        match resolve_ref(&w.constraint, constraints) {
            RefResolution::Resolved(c) => load.waivers.push(WaiverProjection {
                constraint_id: c.id.to_string(),
                constraint_name: c.name.as_deref().map(str::to_string),
                file_path: w.file,
                symbol_qualified_name: w.symbol,
                rationale: w.rationale,
                waived_by: w.by,
            }),
            RefResolution::Unknown => load.warnings.push(warn_for(
                AcceptedWarningKind::UnknownConstraint,
                w.constraint,
                w.file,
            )),
            RefResolution::Ambiguous(_) => load.warnings.push(warn_for(
                AcceptedWarningKind::AmbiguousConstraint,
                w.constraint,
                w.file,
            )),
        }
    }
    for a in file.acks {
        match resolve_ref(&a.constraint, constraints) {
            RefResolution::Resolved(c) => load.acks.push(AckProjection {
                constraint_id: c.id.to_string(),
                constraint_name: c.name.as_deref().map(str::to_string),
                file_path: a.file,
                enclosing_symbol: a.symbol,
                snippet: a.snippet,
                accepted_count: i64::from(a.count),
                rationale: a.rationale,
                acked_by: a.by,
            }),
            RefResolution::Unknown => load.warnings.push(warn_for(
                AcceptedWarningKind::UnknownConstraint,
                a.constraint,
                a.file,
            )),
            RefResolution::Ambiguous(_) => load.warnings.push(warn_for(
                AcceptedWarningKind::AmbiguousConstraint,
                a.constraint,
                a.file,
            )),
        }
    }
    load
}

fn warn_for(kind: AcceptedWarningKind, constraint_ref: String, file: String) -> AcceptedWarning {
    AcceptedWarning {
        kind,
        constraint_ref,
        file,
    }
}

// ---------------------------------------------------------------------------
// Freshness gate
// ---------------------------------------------------------------------------

/// Content hash of the tracked file as it currently sits on disk. Cheap enough
/// (a read + blake3) to call on the guard's hot path to decide staleness without
/// parsing.
pub fn current_file_hash(root: &Path) -> Result<String> {
    Ok(read_file_and_hash(root)?.1)
}

/// True when the DB cache was last projected from the file currently on disk.
/// A `None` marker (never projected) is stale by definition.
pub fn is_cache_fresh(db: &Db, root: &Path) -> Result<bool> {
    let marker = db.get_accepted_sync_marker()?;
    Ok(marker.as_deref() == Some(current_file_hash(root)?.as_str()))
}

/// Server-side freshness gate: parse + resolve the file (so warnings are always
/// current), and re-project the cache only when the on-disk hash differs from the
/// stored marker. Returns the warnings to surface on the report. The parse is
/// microseconds; the freshness check gates only the DB *write*, not the read.
pub fn ensure_cache_fresh(
    db: &Db,
    root: &Path,
    constraints: &[Constraint],
) -> Result<Vec<AcceptedWarning>> {
    let (file, hash) = read_file_and_hash(root)?;
    let stale = db.get_accepted_sync_marker()?.as_deref() != Some(hash.as_str());
    let load = resolve_accepted(file, constraints);
    if stale {
        db.reproject_accepted(&load.waivers, &load.acks, &hash)?;
    }
    Ok(load.warnings)
}

/// Guard-side (read-only) resolution: when the cache is stale the guard cannot
/// write, so it derives the waiver projections in-memory from the same file the
/// server-side reprojection would use — identical data, no persist, no writable
/// handle on the latency path. Acks are report-only, so the guard never needs them.
pub fn resolve_waivers_for_guard(
    root: &Path,
    constraints: &[Constraint],
) -> Result<Vec<WaiverProjection>> {
    let file = load_accepted_file(root)?;
    Ok(resolve_accepted(file, constraints).waivers)
}

// ---------------------------------------------------------------------------
// Mutations (file-first; the caller re-projects the cache after)
// ---------------------------------------------------------------------------

/// Serialize to disk in a canonical, stably-sorted order so mutations produce
/// minimal, review-legible git diffs. Sorts `file` in place (hence `&mut`) rather
/// than cloning it to sort. The write is atomic (temp-file + rename on the same
/// filesystem) so a crash mid-write cannot leave a truncated source of truth.
pub fn write_accepted_file(root: &Path, file: &mut AcceptedFile) -> Result<()> {
    file.waivers
        .sort_by(|a, b| waiver_key(a).cmp(&waiver_key(b)));
    file.acks.sort_by(|a, b| ack_key(a).cmp(&ack_key(b)));
    let toml = toml::to_string_pretty(&*file)
        .map_err(|e| SutraError::Internal(format!("serializing accepted.toml: {e}")))?;
    let path = accepted_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SutraError::Internal(format!("{}: {e}", parent.display())))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &toml)
        .map_err(|e| SutraError::Internal(format!("{}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        SutraError::Internal(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })
}

/// Advisory file lock for the accepted.toml read-modify-write span. Prevents
/// concurrent MCP calls (or a racing `baseline` loop) from producing a lost
/// update. The lock file sits next to the data file (`.sutra/accepted.lock`);
/// the `_guard` is held for the duration of the mutation and released on drop.
///
/// Read-only paths (freshness checks, guard resolution) do NOT take this lock —
/// they derive from whatever state is on disk at the time of read, which is
/// always a complete file thanks to atomic writes.
pub fn lock_accepted(root: &Path) -> Result<std::fs::File> {
    let lock_path = root.join(".sutra").join("accepted.lock");
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| SutraError::Internal(format!("{}: {e}", parent.display())))?;
    }
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .map_err(|e| SutraError::Internal(format!("could not open accepted lock file: {e}")))?;
    lock_file
        .lock_exclusive()
        .map_err(|e| SutraError::Internal(format!("could not acquire accepted lock: {e}")))?;
    Ok(lock_file)
}

type WaiverKey<'a> = (&'a str, &'a str, Option<&'a str>);
fn waiver_key(w: &WaiverEntry) -> WaiverKey<'_> {
    (&w.constraint, &w.file, w.symbol.as_deref())
}

type AckKey<'a> = (&'a str, &'a str, Option<&'a str>, Option<&'a str>);
fn ack_key(a: &AckEntry) -> AckKey<'_> {
    (
        &a.constraint,
        &a.file,
        a.symbol.as_deref(),
        a.snippet.as_deref(),
    )
}

/// Upsert one waiver entry into the file and write it back. The MCP `waive`
/// action calls this, then re-projects — so the acceptance lands in `git diff`
/// immediately, a reviewable event. Holds the advisory file lock for the full
/// read-modify-write span.
pub fn upsert_waiver(root: &Path, entry: WaiverEntry) -> Result<()> {
    let _lock = lock_accepted(root)?;
    let mut file = load_accepted_file(root)?;
    if let Some(existing) = file
        .waivers
        .iter_mut()
        .find(|w| waiver_key(w) == waiver_key(&entry))
    {
        *existing = entry;
    } else {
        file.waivers.push(entry);
    }
    write_accepted_file(root, &mut file)
}

/// Remove a waiver by its `(constraint, file, symbol)` key. Returns whether one
/// was removed. Holds the advisory file lock.
pub fn remove_waiver(
    root: &Path,
    constraint: &str,
    file_path: &str,
    symbol: Option<&str>,
) -> Result<bool> {
    let _lock = lock_accepted(root)?;
    let mut file = load_accepted_file(root)?;
    let before = file.waivers.len();
    let key = (constraint, file_path, symbol);
    file.waivers
        .retain(|w| (w.constraint.as_str(), w.file.as_str(), w.symbol.as_deref()) != key);
    let removed = file.waivers.len() != before;
    if removed {
        write_accepted_file(root, &mut file)?;
    }
    Ok(removed)
}

/// Upsert one ack entry (single `ack`). Holds the advisory file lock.
pub fn upsert_ack(root: &Path, entry: AckEntry) -> Result<()> {
    let _lock = lock_accepted(root)?;
    let mut file = load_accepted_file(root)?;
    if let Some(existing) = file.acks.iter_mut().find(|a| ack_key(a) == ack_key(&entry)) {
        *existing = entry;
    } else {
        file.acks.push(entry);
    }
    write_accepted_file(root, &mut file)
}

/// Batch-upsert multiple ack entries in a single file write cycle. Used by
/// `baseline` to avoid N separate read-modify-write I/O round-trips. Holds the
/// advisory file lock for the entire batch.
pub fn upsert_acks_batch(root: &Path, entries: Vec<AckEntry>) -> Result<()> {
    let _lock = lock_accepted(root)?;
    let mut file = load_accepted_file(root)?;
    for entry in entries {
        if let Some(existing) = file.acks.iter_mut().find(|a| ack_key(a) == ack_key(&entry)) {
            *existing = entry;
        } else {
            file.acks.push(entry);
        }
    }
    write_accepted_file(root, &mut file)
}

/// Remove an ack by its `(constraint, file, symbol, snippet)` key. Returns whether
/// one was removed. Holds the advisory file lock.
pub fn remove_ack(
    root: &Path,
    constraint: &str,
    file_path: &str,
    symbol: Option<&str>,
    snippet: Option<&str>,
) -> Result<bool> {
    let _lock = lock_accepted(root)?;
    let mut file = load_accepted_file(root)?;
    let before = file.acks.len();
    let key = (constraint, file_path, symbol, snippet);
    file.acks.retain(|a| {
        (
            a.constraint.as_str(),
            a.file.as_str(),
            a.symbol.as_deref(),
            a.snippet.as_deref(),
        ) != key
    });
    let removed = file.acks.len() != before;
    if removed {
        write_accepted_file(root, &mut file)?;
    }
    Ok(removed)
}

// ---------------------------------------------------------------------------
// One-time migration
// ---------------------------------------------------------------------------

/// One-time: dump any pre-existing local-only DB waivers/acks (e.g. the 23 chitta
/// waivers) into `accepted.toml` so the move to file-authoritative does not
/// strand them. `Ok(None)` when the file already exists or there is nothing to
/// migrate. Keys by the row's stored `constraint_name`; a row lacking one falls
/// back to its id so nothing is dropped, though such an entry needs a human to
/// re-key it.
pub fn migrate_db_to_file(db: &Db, root: &Path) -> Result<Option<AcceptedFile>> {
    if accepted_path(root).exists() {
        return Ok(None);
    }
    let mut file = AcceptedFile::default();
    for w in db.get_constraint_waivers(None)? {
        let constraint = w
            .constraint_name
            .as_deref()
            .unwrap_or(&w.constraint_id)
            .to_string();
        file.waivers.push(WaiverEntry {
            constraint,
            file: w.file_path,
            symbol: w.symbol_qualified_name,
            rationale: w.rationale,
            by: w.waived_by,
        });
    }
    for a in db.get_all_constraint_instance_acks()? {
        let constraint = a
            .constraint_name
            .as_deref()
            .unwrap_or(&a.constraint_id)
            .to_string();
        file.acks.push(AckEntry {
            constraint,
            file: a.file_path,
            symbol: a.enclosing_symbol,
            snippet: a.snippet,
            count: u32::try_from(a.accepted_count).unwrap_or(1),
            rationale: a.rationale,
            by: a.acked_by,
        });
    }
    if file.waivers.is_empty() && file.acks.is_empty() {
        return Ok(None);
    }
    write_accepted_file(root, &mut file)?;
    Ok(Some(file))
}

/// The read-surface freshness gate, applied at the head of every DD-backed report
/// path (`evaluate_dd`, `orient`, the constraints `list` action). Seeds the file
/// from any legacy DB-only rows (once, gated on file absence) and then re-projects
/// the cache iff the on-disk file changed since the last projection. Returns the
/// warnings (unknown/ambiguous constraint refs) to surface on the report.
///
/// The ordering is load-bearing: `migrate_db_to_file` MUST run before
/// `ensure_cache_fresh`, or a repo whose acceptances still live only in the DB
/// (no file yet) would reproject from an absent -> empty file and wipe them
/// (sutra/308 hazard 1). The mutating actions rely on the same invariant: they
/// migrate before their first `upsert_*` for exactly this reason.
pub fn refresh_cache(
    db: &Db,
    root: &Path,
    constraints: &[Constraint],
) -> Result<Vec<AcceptedWarning>> {
    migrate_db_to_file(db, root)?;
    ensure_cache_fresh(db, root, constraints)
}

/// Guard-side freshness check against a read-only connection. `evaluate_raw`
/// (the guard's edit-time path) holds a bare `Connection`, not a writable `Db`,
/// so it cannot reproject; it uses this to decide whether the cache it is about
/// to read is coherent with the file on disk, falling back to an in-memory
/// resolution ([`resolve_waivers_for_guard`]) when it is not.
pub fn is_cache_fresh_conn(conn: &rusqlite::Connection, root: &Path) -> Result<bool> {
    let marker = crate::db::accepted_sync_marker_from_conn(conn);
    Ok(marker.as_deref() == Some(current_file_hash(root)?.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn constraints(toml: &str) -> Vec<Constraint> {
        let mut rules = crate::rules::parse_rules(toml).expect("valid rules toml");
        rules.all_constraints().0
    }

    fn one_named(name: &str) -> Vec<Constraint> {
        constraints(&format!(
            "[[constraint]]\nkind = \"no_cycles\"\nname = \"{name}\"\nscope = \"src/\"\n"
        ))
    }

    fn waiver(constraint: &str, file: &str) -> WaiverEntry {
        WaiverEntry {
            constraint: constraint.to_string(),
            file: file.to_string(),
            symbol: None,
            rationale: "examined; fine".to_string(),
            by: "josh".to_string(),
        }
    }

    #[test]
    fn resolve_ref_distinguishes_unknown_resolved_ambiguous() {
        let cs = one_named("no-loops");
        assert!(matches!(
            resolve_ref("no-loops", &cs),
            RefResolution::Resolved(_)
        ));
        assert!(matches!(resolve_ref("nope", &cs), RefResolution::Unknown));
    }

    #[test]
    fn roundtrip_write_then_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = AcceptedFile::default();
        f.waivers.push(waiver("no-loops", "src/a.rs"));
        f.acks.push(AckEntry {
            constraint: "no-loops".to_string(),
            file: "src/b.rs".to_string(),
            symbol: Some("do_it".to_string()),
            snippet: Some("x.clone()".to_string()),
            count: 3,
            rationale: None,
            by: "josh".to_string(),
        });
        write_accepted_file(dir.path(), &mut f).unwrap();
        let back = load_accepted_file(dir.path()).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn absent_file_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = load_accepted_file(dir.path()).unwrap();
        assert!(f.waivers.is_empty() && f.acks.is_empty());
    }

    #[test]
    fn malformed_file_fails_loud() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".sutra")).unwrap();
        std::fs::write(accepted_path(dir.path()), "this is not = valid = toml").unwrap();
        assert!(load_accepted_file(dir.path()).is_err());
    }

    #[test]
    fn unknown_constraint_becomes_warning_not_drop() {
        let cs = one_named("no-loops");
        let mut f = AcceptedFile::default();
        f.waivers.push(waiver("was-renamed", "src/a.rs"));
        let load = resolve_accepted(f, &cs);
        assert!(load.waivers.is_empty());
        assert_eq!(load.warnings.len(), 1);
        assert_eq!(
            load.warnings[0].kind,
            AcceptedWarningKind::UnknownConstraint
        );
    }

    #[test]
    fn ensure_cache_fresh_projects_then_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let db = Db::open_unchecked("test", root).unwrap();
        let cs = one_named("no-loops");

        let mut f = AcceptedFile::default();
        f.waivers.push(waiver("no-loops", "src/a.rs"));
        write_accepted_file(root, &mut f).unwrap();

        // First gate: stale marker -> projects the row and stamps the hash.
        let warns = ensure_cache_fresh(&db, root, &cs).unwrap();
        assert!(warns.is_empty());
        assert_eq!(db.get_constraint_waivers(None).unwrap().len(), 1);
        assert!(is_cache_fresh(&db, root).unwrap());

        // Second gate: marker matches -> no re-projection, no duplication.
        ensure_cache_fresh(&db, root, &cs).unwrap();
        assert_eq!(db.get_constraint_waivers(None).unwrap().len(), 1);
    }

    #[test]
    fn edited_file_reprojects_and_drops_removed_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let db = Db::open_unchecked("test", root).unwrap();
        let cs = one_named("no-loops");

        let mut f = AcceptedFile::default();
        f.waivers.push(waiver("no-loops", "src/a.rs"));
        f.waivers.push(waiver("no-loops", "src/b.rs"));
        write_accepted_file(root, &mut f).unwrap();
        ensure_cache_fresh(&db, root, &cs).unwrap();
        assert_eq!(db.get_constraint_waivers(None).unwrap().len(), 2);

        // Remove one entry from the file; the cache must shrink to match.
        remove_waiver(root, "no-loops", "src/b.rs", None).unwrap();
        assert!(!is_cache_fresh(&db, root).unwrap());
        ensure_cache_fresh(&db, root, &cs).unwrap();
        assert_eq!(db.get_constraint_waivers(None).unwrap().len(), 1);
    }

    #[test]
    fn guard_resolution_matches_server_projection() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cs = one_named("no-loops");
        let mut f = AcceptedFile::default();
        f.waivers.push(waiver("no-loops", "src/a.rs"));
        write_accepted_file(root, &mut f).unwrap();

        let guard = resolve_waivers_for_guard(root, &cs).unwrap();
        let server = resolve_accepted(load_accepted_file(root).unwrap(), &cs).waivers;
        assert_eq!(guard.len(), 1);
        assert_eq!(guard.len(), server.len());
        assert_eq!(guard[0].constraint_id, server[0].constraint_id);
    }

    #[test]
    fn upsert_replaces_same_key_not_appends() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        upsert_waiver(root, waiver("no-loops", "src/a.rs")).unwrap();
        let mut updated = waiver("no-loops", "src/a.rs");
        updated.rationale = "revised reason".to_string();
        upsert_waiver(root, updated).unwrap();
        let f = load_accepted_file(root).unwrap();
        assert_eq!(f.waivers.len(), 1);
        assert_eq!(f.waivers[0].rationale, "revised reason");
    }
}
