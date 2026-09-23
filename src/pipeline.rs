//! Parse pipeline: walk workspace, parse files, resolve refs, compute rollups.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fs2::FileExt;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::components;
use crate::config::Config;
use crate::db::{
    Db, ResolvedRefRow, SnapshotCompleteness, SnapshotComponentRow, SnapshotFileRow, SnapshotParams,
};
use crate::error::Result;
use crate::graph;
use crate::parser;
use crate::parser::adapter::{LanguageRegistry, ParserPool};
use crate::resolver;
use crate::workspace::WorkspaceEntry;

/// Shared per-workspace parse lock. MCP tool handlers acquire the lock before
/// parsing, preventing concurrent parses against the same SQLite database.
///
/// The `locks` mutex serializes *all* work that must not overlap a parse —
/// since sutra/298 that includes DD-backed constraint evaluation, which holds
/// the lock without parsing. The separate `parsing` flag records whether a
/// *genuine parse* is in flight, so `parsing_in_progress` freshness signals
/// don't report an evaluation lock holder as a parse (sutra/300). Mark it via
/// [`ParseCoordinator::mark_parsing`] at the parse sites; read it via
/// [`ParseCoordinator::is_parsing`].
#[derive(Clone, Default)]
pub struct ParseCoordinator {
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    parsing: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

/// RAII guard that clears the per-workspace parsing flag on drop. Held for the
/// duration of a genuine parse (alongside the parse lock guard).
pub struct ParseMark(Arc<AtomicBool>);

impl Drop for ParseMark {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl ParseCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lock_for(&self, ws_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.locks
                .lock()
                .entry(ws_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// Marks a genuine parse as in progress for this workspace and returns a
    /// guard that clears the mark on drop. Call at the parse sites (holding the
    /// parse lock), NOT in the evaluation-serialization path.
    pub fn mark_parsing(&self, ws_id: &str) -> ParseMark {
        let mut parsing = self.parsing.lock();
        let flag = parsing
            .entry(ws_id.to_string())
            .or_insert_with(|| Arc::new(AtomicBool::new(false)));
        flag.store(true, Ordering::SeqCst);
        ParseMark(Arc::clone(flag))
    }

    /// Returns `true` if a genuine parse is currently running for this
    /// workspace. Unlike a raw lock check, this excludes evaluation lock
    /// holders (sutra/300).
    pub fn is_parsing(&self, ws_id: &str) -> bool {
        let parsing = self.parsing.lock();
        match parsing.get(ws_id) {
            Some(flag) => flag.load(Ordering::SeqCst),
            None => false,
        }
    }
}

/// Open (creating if needed) the per-workspace `parse.lock` file *without*
/// acquiring the flock. Both the blocking [`acquire_parse_flock`] and the
/// nonblocking [`try_acquire_parse_flock`] share this so they open the identical
/// descriptor and only differ in how they take the OS lock.
fn open_parse_lock_file(config: &Config, workspace_id: &str) -> Result<std::fs::File> {
    let lock_dir = config.db_dir.join(workspace_id);
    std::fs::create_dir_all(&lock_dir).map_err(|e| {
        crate::error::SutraError::Internal(format!(
            "could not create lock directory {}: {e}",
            lock_dir.display()
        ))
    })?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_dir.join("parse.lock"))
        .map_err(|e| {
            crate::error::SutraError::Internal(format!("could not open parse lock file: {e}"))
        })
}

/// Acquire the cross-process parse flock, *blocking* until it is free. Owned by
/// full and incremental parse — the writers that may wait for a peer to finish.
fn acquire_parse_flock(config: &Config, workspace_id: &str) -> Result<std::fs::File> {
    let lock_file = open_parse_lock_file(config, workspace_id)?;
    lock_file.lock_exclusive().map_err(|e| {
        crate::error::SutraError::Internal(format!("could not acquire parse lock: {e}"))
    })?;
    Ok(lock_file)
}

/// Try to acquire the cross-process parse flock *without blocking on a peer's
/// write*. Returns `Ok(None)` when the flock is genuinely held (the caller must
/// defer rather than wait). Used by the demand health refresh, which the contract
/// requires to report `LockBusy` on contention instead of blocking a health query
/// behind a peer's write (`docs/health-evidence-contract.md`, "Publication and
/// consumers").
///
/// The acquire retries over a tiny bounded budget before reporting contention.
/// This is not a wait on a peer write — it rides out a *spurious* contention
/// window unrelated to any lock owner: whenever any thread in this process spawns
/// a subprocess (git, etc.), `fork` duplicates every open fd — including a peer
/// workspace's parse-lock fd — into the child, and the flock on that shared open
/// file description stays held until the child reaches `exec` and `O_CLOEXEC`
/// drops the fd. During that sub-millisecond fork→exec window a bare
/// `try_lock_exclusive` observes contention even though the flock's real owner has
/// already released it. A genuine peer parse holds the flock for the whole run
/// (far longer than the budget), so it still exhausts every attempt and the caller
/// defers as required (sutra/428).
pub fn try_acquire_parse_flock(
    config: &Config,
    workspace_id: &str,
) -> Result<Option<std::fs::File>> {
    /// Attempts spread across the transient fork→exec window; the final attempt
    /// does not sleep, so the worst-case added latency is `(ATTEMPTS-1) * BACKOFF`.
    const ATTEMPTS: u32 = 5;
    const BACKOFF: Duration = Duration::from_millis(1);

    let lock_file = open_parse_lock_file(config, workspace_id)?;
    for attempt in 0..ATTEMPTS {
        match lock_file.try_lock_exclusive() {
            Ok(()) => return Ok(Some(lock_file)),
            // Contention is not an error. It may be a spurious fork→exec blip (retry
            // to ride it out) or a genuine peer write (retries exhaust → defer).
            Err(e) if e.kind() == fs2::lock_contended_error().kind() => {
                if attempt + 1 < ATTEMPTS {
                    std::thread::sleep(BACKOFF);
                }
            }
            // Any other error is a real filesystem/lock failure.
            Err(e) => {
                return Err(crate::error::SutraError::Internal(format!(
                    "could not attempt parse lock: {e}"
                )));
            }
        }
    }
    Ok(None)
}

/// Summary of a parse pipeline run.
#[derive(Debug, Clone)]
pub struct ParseSnapshot {
    pub files_walked: i64,
    pub files_parsed: i64,
    pub symbols_extracted: i64,
    pub refs_extracted: i64,
    pub parse_errors: i64,
    pub duration_ms: i64,
    pub resolved_count: i64,
    pub unresolved_count: i64,
    /// Refs where resolution was skipped (Import context).
    pub skipped_count: i64,
}

/// Current resident set size in MB, read from `/proc/self/statm` (Linux).
/// Instrumentation only — returns `None` off Linux or on read failure. Used to
/// attribute reparse RSS to a phase of `post_parse_sequence` (sutra/324); view
/// with `RUST_LOG=sutra=debug`.
/// Minimum interval between periodic progress lines in the parse and
/// resolution loops, so multi-hour runs are distinguishable from stalls.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// Bulk mode activates above this many files in a loop. Small parses keep
/// per-file commits (finer crash granularity); large parses share one
/// transaction per batch of files to cut per-file WAL fsyncs.
const BULK_MODE_THRESHOLD: usize = 1000;
const BULK_BATCH_FILES: usize = 64;
/// Checkpoint-truncate the WAL every N committed batches: continuous write
/// transactions starve SQLite's passive auto-checkpoint, and kala-reverse
/// grew a 798MB WAL with no bound.
const BULK_CHECKPOINT_EVERY_BATCHES: usize = 16;

/// RAII bulk-batch handle over `Db::begin_batch`/`commit_batch`: commits
/// every `BULK_BATCH_FILES` files, rolls an open batch back on drop so the
/// error path never leaves a transaction dangling.
struct BulkBatch<'a> {
    db: &'a Db,
    active: bool,
    open: bool,
    files_in_batch: usize,
    batches_committed: usize,
}

impl<'a> BulkBatch<'a> {
    fn new(db: &'a Db, active: bool) -> Self {
        Self {
            db,
            active,
            open: false,
            files_in_batch: 0,
            batches_committed: 0,
        }
    }

    fn file_start(&mut self) -> Result<()> {
        if self.active && !self.open {
            self.db.begin_batch()?;
            self.open = true;
        }
        Ok(())
    }

    fn file_done(&mut self) -> Result<()> {
        if self.active {
            self.files_in_batch += 1;
            if self.files_in_batch >= BULK_BATCH_FILES {
                self.commit()?;
            }
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        if self.open {
            self.db.commit_batch()?;
            self.open = false;
            self.files_in_batch = 0;
            self.batches_committed += 1;
            if self
                .batches_committed
                .is_multiple_of(BULK_CHECKPOINT_EVERY_BATCHES)
                && let Err(e) = self.db.wal_checkpoint_truncate()
            {
                debug!("wal checkpoint skipped: {e}");
            }
        }
        Ok(())
    }
}

impl Drop for BulkBatch<'_> {
    fn drop(&mut self) {
        if self.open
            && let Err(e) = self.db.rollback_batch()
        {
            warn!("bulk batch rollback failed: {e}");
        }
    }
}

fn current_rss_mb() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    // statm reports pages; the Linux page size is 4 KiB on all targets we run on.
    Some(resident_pages.saturating_mul(4096) / (1024 * 1024))
}

/// Log RSS at a named phase boundary. No-op formatting cost when `debug` is off.
fn log_phase_rss(phase: &str) {
    if let Some(mb) = current_rss_mb() {
        debug!(phase, rss_mb = mb, "reparse memory");
    }
}

/// Build-output directories to skip even when a workspace does not gitignore
/// them. Hidden dirs (`.git`, `.claude`, …) are already pruned by the walker's
/// `hidden` filter; these are the non-hidden ones worth hard-skipping.
pub(crate) const SKIP_DIRS: &[&str] = &[
    "target",
    "build",
    "node_modules",
    ".git",
    "dist",
    "out",
    "vendor",
    "__pycache__",
    ".claude",
];

/// A workspace directory walker that respects `.gitignore` (plus `.ignore`,
/// parent gitignores, `.git/info/exclude`, and the global gitignore), skips
/// hidden entries, and hard-skips build-output dirs in [`SKIP_DIRS`].
///
/// `require_git(false)` makes `.gitignore` files apply even when the workspace
/// is not itself a git checkout, so "respect .gitignore" holds regardless of
/// whether `.git` is present. Symlinks are not followed (git/ripgrep default).
pub(crate) fn workspace_walker(root: &Path) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(root);
    builder.require_git(false).filter_entry(|entry| {
        // Prune known build outputs that a project may not have gitignored.
        !(entry.file_type().is_some_and(|ft| ft.is_dir())
            && SKIP_DIRS.contains(&entry.file_name().to_string_lossy().as_ref()))
    });
    builder
}

struct FileParseResult {
    file_id: i64,
    symbols_extracted: i64,
    refs_extracted: i64,
    parse_errors: i64,
    /// True when this call returned without re-extracting the file while a prior
    /// index row for it still exists — a read failure, a parse failure, or an
    /// oversized-file skip. Under a parser-identity forced reparse (sutra/364,
    /// sutra/381) such a row still reflects the OLD extractor, so the caller must
    /// not advance the parser stamp: the file was not covered by this extractor.
    retained_stale_row: bool,
}

enum PostParseResult {
    Full {
        resolved_count: i64,
        unresolved_count: i64,
        skipped_count: i64,
    },
    NoChanges,
}

fn parse_single_file(
    db: &Db,
    file_path: &Path,
    workspace_root: &Path,
    registry: &LanguageRegistry,
    pool: &mut ParserPool,
    trust_mtime: bool,
    force_reparse: bool,
) -> Result<Option<FileParseResult>> {
    let rel_path = file_path
        .strip_prefix(workspace_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .to_string();

    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let adapter = match registry.adapter_for_extension(ext) {
        Some(a) => a,
        None => return Ok(None),
    };
    let language = adapter.language_id();

    // Cheap stat before reading: on a frozen-workspace reparse an unchanged file
    // (mtime matches the stored baseline) is skipped without reading its bytes,
    // so ingesting a few new files into a large corpus doesn't re-read the whole
    // corpus (sutra/324). Non-frozen workspaces don't trust mtime — a mtime-
    // preserving edit must still be caught by the content-hash check below.
    let meta = std::fs::metadata(file_path).ok();
    let mtime_ns: Option<i64> = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64);
    // Size baseline paired with mtime for the freshness drift probe (sutra/362).
    let size_bytes: Option<i64> = meta.as_ref().map(|m| m.len() as i64);

    let existing = db.file_by_path(&rel_path)?;
    // A parser-identity change (sutra/364) invalidates every skip: the bytes are
    // unchanged but the extractor that produced the stored symbols is not, so
    // neither the mtime nor the content-hash short-circuit may fire. That stamp
    // is the only extractor-staleness signal: an earlier "NULL language_attrs
    // means pre-migration row" gate re-extracted every file holding a symbol
    // kind whose extractor legitimately emits no attrs, on every parse (sutra/431).
    if !force_reparse
        && trust_mtime
        && let Some(ns) = mtime_ns
        && let Some(ref ex) = existing
        && ex.mtime_ns == Some(ns)
    {
        return Ok(None);
    }

    let contents = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(path = %rel_path, error = %e, "could not read file");
            return Ok(Some(FileParseResult {
                file_id: 0,
                symbols_extracted: 0,
                refs_extracted: 0,
                parse_errors: 1,
                retained_stale_row: existing.is_some(),
            }));
        }
    };

    let line_count = contents.lines().count();
    if line_count > parser::persist::MAX_LINES {
        warn!(path = %rel_path, lines = line_count, max = parser::persist::MAX_LINES, "file exceeds line limit, skipping");
        // An oversized file is not re-extracted. If it already has an index row
        // (it shrank under the limit once, then grew past it), that row survives
        // and must block a parser-stamp advance the same as a parse failure
        // (sutra/381). Report it rather than an opaque skip.
        return Ok(Some(FileParseResult {
            file_id: 0,
            symbols_extracted: 0,
            refs_extracted: 0,
            parse_errors: 0,
            retained_stale_row: existing.is_some(),
        }));
    }

    let content_hash = blake3::hash(contents.as_bytes()).to_hex().to_string();

    if !force_reparse
        && let Some(ref ex) = existing
        && ex.content_hash == content_hash
    {
        return Ok(None);
    }

    // Parse before deleting old data — on failure, keep the existing index intact.
    let parse_result = match pool.parse_with(adapter, &contents, &rel_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(path = %rel_path, error = %e, "parse failed, keeping existing index");
            return Ok(Some(FileParseResult {
                file_id: 0,
                symbols_extracted: 0,
                refs_extracted: 0,
                parse_errors: 1,
                retained_stale_row: existing.is_some(),
            }));
        }
    };

    let mut parse_errors: i64 = 0;
    if !parse_result.parsed_ok {
        parse_errors = 1;
    }

    let (flat_symbols, parent_indices) =
        parser::persist::flatten_symbols_for_insert(&parse_result.symbols);
    let import_params = parser::persist::build_import_params(&parse_result);
    let ref_params = parser::persist::build_ref_params(&parse_result, &rel_path);

    let (file_id, symbols_extracted) = db.replace_file_data(
        &rel_path,
        language,
        &content_hash,
        line_count as i64,
        parse_result.parsed_ok,
        mtime_ns,
        size_bytes,
        &flat_symbols,
        &parent_indices,
        &import_params,
        &ref_params,
    )?;
    let refs_extracted = ref_params.len() as i64;

    Ok(Some(FileParseResult {
        file_id,
        symbols_extracted,
        refs_extracted,
        parse_errors,
        // replace_file_data ran: the row now reflects this binary's extractor.
        retained_stale_row: false,
    }))
}

fn resolve_file_refs(
    db: &Db,
    file_id: i64,
    index: &resolver::SymbolIndex<'_>,
) -> Result<(i64, i64, i64)> {
    let file_symbols_rows = db.find_symbols_by_file(file_id)?;
    let file_refs = db.find_refs_in_file(file_id)?;
    let file_imports = db.imports_for_file(file_id)?;

    if file_refs.is_empty() {
        db.replace_refs_and_clear_resolution(file_id, &[])?;
        return Ok((0, 0, 0));
    }

    let extracted_symbols: Vec<parser::ExtractedSymbol> = file_symbols_rows
        .iter()
        .map(|s| parser::ExtractedSymbol {
            qualified_name: s.qualified_name.to_string(),
            short_name: s.short_name.to_string(),
            kind: parse_symbol_kind(&s.kind),
            signature: s.signature.clone(),
            signature_hash: s.signature_hash.clone(),
            structural_hash: s.structural_hash.clone(),
            visibility: s.visibility.clone(),
            start_line: s.start_line as usize,
            start_col: s.start_col as usize,
            end_line: s.end_line as usize,
            end_col: s.end_col as usize,
            children: vec![],
            parent_symbol_id: s.parent_symbol_id,
            docstring: s.docstring.clone(),
            cyclomatic: s.cyclomatic.map(|v| v as u32),
            cognitive: s.cognitive.map(|v| v as u32),
            max_nesting: s.max_nesting.map(|v| v as u32),
            flags: 0,
            language_attrs: None,
        })
        .collect();

    // Carry through refs that were resolved in a prior pass and no longer carry
    // a call-site name (unresolved_name is cleared on resolution). They point at
    // unchanged symbols in other files; re-running the resolver on them would
    // drop them — there is no name left to match — so preserve them verbatim.
    // Only name-bearing refs are (re)resolved. On a full parse every ref is
    // freshly extracted with a name, so nothing is carried and behaviour is
    // unchanged; this only fires on the incremental query path when a caller is
    // re-resolved without being re-extracted (sutra/378).
    let (carried, resolvable): (Vec<&crate::db::RefRow>, Vec<&crate::db::RefRow>) = file_refs
        .iter()
        .partition(|r| r.target_symbol_id.is_some() && r.unresolved_name.is_none());

    let extracted_refs: Vec<parser::ExtractedRef> = resolvable
        .iter()
        .map(|r| parser::ExtractedRef {
            name: r.unresolved_name.clone().unwrap_or_default(),
            line: r.line as usize,
            col: r.col as usize,
            context_kind: parse_ref_context_kind(&r.context_kind),
            resolved_local_target: r.resolved_local_target.clone(),
            receiver: r.receiver.clone(),
        })
        .collect();

    let extracted_imports: Vec<parser::ExtractedImport> = file_imports
        .iter()
        .map(|i| parser::ExtractedImport {
            raw_path: i.imported_path.clone(),
            line: i.line as usize,
            kind: "import",
            alias: i.alias.clone(),
            is_test: i.is_test,
        })
        .collect();

    // Language gates the Python class-as-constructor rule in the resolver;
    // absent a file row (should not happen post-parse) fall back to a neutral
    // value so the general resolution path is unaffected.
    let language = db
        .file_by_id(file_id)?
        .map(|f| f.language)
        .unwrap_or_default();

    let resolved = resolver::resolve_refs(
        &extracted_symbols,
        &extracted_refs,
        index,
        &extracted_imports,
        file_id,
        &language,
    );

    let mut ref_rows: Vec<ResolvedRefRow<'_>> = resolved
        .iter()
        .map(|rr| ResolvedRefRow {
            target_symbol_id: rr.target_symbol_id,
            unresolved_name: rr.unresolved_name.as_deref(),
            line: rr.original.line as i64,
            col: rr.original.col as i64,
            context_kind: rr.original.context_kind.as_str(),
            resolution_method: rr.resolution_method.map(|m| m.as_str()),
            resolved_local_target: rr.original.resolved_local_target.as_deref(),
            receiver: rr.original.receiver.as_deref(),
        })
        .collect();

    // Preserve the carried-through resolved rows verbatim (resolution_method is
    // write-only diagnostic metadata, not read by any query, so it is dropped).
    ref_rows.extend(carried.iter().map(|r| ResolvedRefRow {
        target_symbol_id: r.target_symbol_id,
        unresolved_name: r.unresolved_name.as_deref(),
        line: r.line,
        col: r.col,
        context_kind: r.context_kind.as_str(),
        resolution_method: None,
        resolved_local_target: r.resolved_local_target.as_deref(),
        receiver: r.receiver.as_deref(),
    }));

    db.replace_refs_and_clear_resolution(file_id, &ref_rows)?;

    let mut resolved_count: i64 = 0;
    let mut unresolved: i64 = 0;
    let mut skipped: i64 = 0;
    for rr in &resolved {
        if rr.skipped {
            skipped += 1;
        } else if rr.target_symbol_id.is_none() {
            unresolved += 1;
        } else {
            resolved_count += 1;
        }
    }
    // Carried refs kept their existing resolution.
    resolved_count += carried.len() as i64;

    Ok((resolved_count, unresolved, skipped))
}

/// Drop indexed files that the current walk no longer yields. This covers both
/// files deleted from disk and files that are still present but newly excluded —
/// e.g. code that a `.gitignore` rule now hides. `present` is the set of absolute
/// paths returned by [`walk_source_files`] for this workspace.
fn prune_stale_files(db: &Db, workspace_root: &Path, present: &[std::path::PathBuf]) -> usize {
    let files = match db.all_files() {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let present: HashSet<&Path> = present.iter().map(|p| p.as_path()).collect();
    let mut count = 0;
    for file in &files {
        let abs = workspace_root.join(&*file.path);
        if !present.contains(abs.as_path()) {
            if let Err(e) = db.delete_file_cascade(file.id) {
                warn!(file = %file.path, "failed to prune stale file: {e}");
            } else {
                count += 1;
            }
        }
    }
    count
}

pub fn parse_workspace(
    workspace: &WorkspaceEntry,
    db: &Db,
    config: &Config,
    cancel: &AtomicBool,
    registry: &LanguageRegistry,
) -> Result<ParseSnapshot> {
    let flock = acquire_parse_flock(config, &workspace.id)?;
    // The full parse holds the coordinator (its caller) + this flock, so it is
    // the already-locked health publisher (sutra/415).
    let health_session = crate::health::refresh::HealthSession::from_held_flock(&flock);
    let head_commit = crate::git::head_commit_hash(&workspace.root);
    let parse_started_at = chrono::Utc::now().to_rfc3339();
    let start = Instant::now();
    let mut pool = ParserPool::new(Duration::from_millis(config.parse_timeout_ms));

    let allowed_extensions: Vec<&str> = registry.extensions_for_languages(&workspace.languages);

    let (source_files, walk_complete) =
        walk_source_files_checked(&workspace.root, &allowed_extensions);
    info!(workspace = %workspace.id, files_found = source_files.len(), "walked workspace");

    // Prune indexed files the walk no longer yields — deleted from disk or newly
    // excluded (e.g. now covered by a `.gitignore` rule). Only ever from a
    // provably complete walk: a partial walk under-yields paths, so pruning on
    // its shortfall would cascade-delete live files (sutra/379).
    let pruned = if walk_complete {
        let pruned = prune_stale_files(db, &workspace.root, &source_files);
        if pruned > 0 {
            info!(workspace = %workspace.id, pruned, "pruned stale files from index");
        }
        pruned
    } else {
        warn!(
            workspace = %workspace.id,
            "workspace walk hit entry errors; skipping stale-file prune to avoid deleting live index rows"
        );
        0
    };

    if source_files.is_empty()
        && workspace.root.is_dir()
        && std::fs::read_dir(&workspace.root).is_ok_and(|mut d| d.next().is_some())
    {
        warn!(
            workspace = %workspace.id,
            root = %workspace.root.display(),
            languages = ?workspace.languages,
            "walk found 0 source files — check workspace languages config"
        );
    }

    // Parser-identity gate (sutra/364): if the extractor that built the index
    // differs from this binary's — a grammar bump, adapter fix, or symbol-kind
    // change, all invisible to the per-file content_hash — force one full
    // re-extraction so no unchanged file replays stale symbols. `None` (an index
    // built before the stamp existed) reads as a mismatch and heals the same way.
    let force_reparse = db.parser_stamp().unwrap_or(None).as_deref() != Some(parser::PARSER_STAMP);
    if force_reparse {
        info!(
            workspace = %workspace.id,
            "parser identity changed since last parse; forcing full re-extraction"
        );
    }

    let mut files_parsed: i64 = 0;
    let mut symbols_extracted: i64 = 0;
    let mut refs_extracted: i64 = 0;
    let mut parse_errors: i64 = 0;
    // Set when any walked file kept a pre-existing index row instead of being
    // re-extracted (read/parse failure, oversized skip). A forced reparse that
    // leaves such rows has NOT re-extracted the whole index, so the parser stamp
    // must not advance (sutra/381).
    let mut left_stale_rows = false;
    let inner = (|| -> Result<PostParseResult> {
        let mut last_progress = Instant::now();
        let mut last_progress_walked: usize = 0;
        let mut bulk = BulkBatch::new(db, source_files.len() >= BULK_MODE_THRESHOLD);
        for (walked, file_path) in source_files.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err(crate::error::SutraError::Internal("parse cancelled".into()));
            }
            bulk.file_start()?;
            if let Some(result) = parse_single_file(
                db,
                file_path,
                &workspace.root,
                registry,
                &mut pool,
                workspace.frozen,
                force_reparse,
            )? {
                parse_errors += result.parse_errors;
                if result.retained_stale_row {
                    left_stale_rows = true;
                }
                if result.file_id != 0 {
                    files_parsed += 1;
                    symbols_extracted += result.symbols_extracted;
                    refs_extracted += result.refs_extracted;
                }
            }
            bulk.file_done()?;
            if last_progress.elapsed() >= PROGRESS_INTERVAL {
                let window_secs = last_progress.elapsed().as_secs_f64();
                let recent_per_min =
                    (walked + 1 - last_progress_walked) as f64 / window_secs * 60.0;
                let remaining = source_files.len() - (walked + 1);
                let eta_s = if recent_per_min > 0.0 {
                    (remaining as f64 / recent_per_min * 60.0) as u64
                } else {
                    0
                };
                info!(
                    workspace = %workspace.id,
                    files = walked + 1,
                    files_total = source_files.len(),
                    parsed = files_parsed,
                    symbols = symbols_extracted,
                    refs = refs_extracted,
                    elapsed_s = start.elapsed().as_secs(),
                    recent_files_per_min = format!("{recent_per_min:.1}"),
                    eta_s,
                    "parse progress"
                );
                last_progress = Instant::now();
                last_progress_walked = walked + 1;
            }
        }

        bulk.commit()?;

        if files_parsed == 0 && parse_errors == 0 && pruned == 0 && !db.has_pending_work()? {
            return Ok(PostParseResult::NoChanges);
        }

        let (resolved_count, unresolved_count, skipped_count) = post_parse_sequence(
            db,
            &workspace.root,
            &registry.boundary_multipliers(),
            registry,
            &health_session,
        )?;
        Ok(PostParseResult::Full {
            resolved_count,
            unresolved_count,
            skipped_count,
        })
    })();

    let duration_ms = start.elapsed().as_millis() as i64;
    let recorded_errors = match &inner {
        Ok(_) => parse_errors,
        Err(_) => parse_errors.max(1),
    };
    let meta = CheckpointMeta {
        head_commit,
        timestamp: Some(parse_started_at),
        files_parsed,
        symbols_extracted,
        refs_extracted,
        parse_errors: recorded_errors,
        duration_ms,
    };
    match &inner {
        Ok(PostParseResult::NoChanges) => {
            // Checkpoint only after publication (health-evidence contract): an
            // unchanged-source parse still refreshes health under the held lock,
            // so a crossed midnight / HEAD move / config edit republishes rather
            // than checkpointing a stale (all-partial) run.
            let now = chrono::Utc::now().timestamp();
            if let Err(e) =
                crate::health::refresh::refresh(&health_session, db, &workspace.root, now)
            {
                warn!(workspace = %workspace.id, "health refresh failed on unchanged parse: {e}");
            }
            if let Err(e) = record_unchanged_snapshot(db, &workspace.root, meta) {
                warn!(workspace = %workspace.id, "failed to record unchanged snapshot after parse: {e}");
            }
        }
        Ok(PostParseResult::Full { .. }) => {
            if let Err(e) = record_snapshot(db, &workspace.root, meta) {
                warn!(workspace = %workspace.id, "failed to record snapshot after parse: {e}");
            } else if let Err(e) = db
                .get_data_generation()
                .and_then(|g| db.set_derived_complete(g))
            {
                warn!(workspace = %workspace.id, "failed to mark derived data complete: {e}");
            }
        }
        Err(_) => {
            if let Err(e) = record_snapshot(db, &workspace.root, meta) {
                warn!(workspace = %workspace.id, "failed to record snapshot after parse: {e}");
            }
        }
    }

    // Record the extractor identity only when this parse actually covered the
    // whole index with the current extractor (sutra/364, sutra/381). `inner.is_ok`
    // is necessary but not sufficient: a per-file read/parse failure or an
    // oversized skip returns Ok while keeping the file's OLD row (`left_stale_rows`),
    // and an incomplete walk (`!walk_complete`) never visits some on-disk files at
    // all — both leave old-extractor symbols in place. Advancing the stamp then
    // would let the next parse trust the content_hash skip over those stale rows
    // forever. When a forced reparse can't prove full coverage, leave the old
    // stamp so the next parse re-heals. (When not forced the stamp already matches,
    // so re-writing it is a harmless no-op — the coverage gate only bites a heal.)
    let extraction_covered_index = walk_complete && !left_stale_rows;
    if inner.is_ok() && (!force_reparse || extraction_covered_index) {
        if let Err(e) = db.set_parser_stamp(parser::PARSER_STAMP) {
            warn!(workspace = %workspace.id, "failed to record parser stamp after parse: {e}");
        }
    } else if force_reparse && inner.is_ok() {
        warn!(
            workspace = %workspace.id,
            walk_complete,
            left_stale_rows,
            "parser identity heal incomplete; leaving prior stamp so the next parse retries the uncovered files"
        );
    }

    let (resolved_count, unresolved_count, skipped_count) = match inner? {
        PostParseResult::Full {
            resolved_count,
            unresolved_count,
            skipped_count,
        } => (resolved_count, unresolved_count, skipped_count),
        PostParseResult::NoChanges => (0, 0, 0),
    };

    Ok(ParseSnapshot {
        files_walked: source_files.len() as i64,
        files_parsed,
        symbols_extracted,
        refs_extracted,
        parse_errors,
        duration_ms,
        resolved_count,
        unresolved_count,
        skipped_count,
    })
}

fn entity_change_walk(db: &Db, workspace_root: &Path, max_commits: u32) -> Result<usize> {
    use crate::db::entity_changes::EntityChangeRow;
    use crate::tools::symbol_diff::{self, ChangeKind};

    let known = db.known_entity_commit_hashes()?;

    let commits = match crate::git::git_first_parent_commits(workspace_root, max_commits) {
        Ok(c) => c,
        Err(e) => {
            warn!("entity walk: git rev-list failed: {e}");
            return Ok(0);
        }
    };

    let new_commits: Vec<_> = commits
        .into_iter()
        .filter(|c| !known.contains(&c.hash))
        .collect();

    if new_commits.is_empty() {
        return Ok(0);
    }

    let mut total_changes = 0usize;

    for commit in &new_commits {
        if commit.is_merge {
            db.insert_entity_commit_with_changes(
                &commit.hash,
                commit.timestamp,
                &commit.author,
                &[],
            )?;
            continue;
        }

        let changed_files = match crate::git::git_commit_changed_files(workspace_root, &commit.hash)
        {
            Ok(f) => f,
            Err(e) => {
                warn!(hash = %commit.hash, "entity walk: skipping commit (diff-tree failed: {e})");
                continue;
            }
        };

        let parent = format!("{}~1", commit.hash);
        let mut changes = Vec::new();
        let mut seen_keys: HashSet<String> = HashSet::new();

        for entry in &changed_files {
            let symbol_changes = match symbol_diff::diff_file(
                workspace_root,
                &entry.path,
                entry.old_path.as_deref(),
                &parent,
                &commit.hash,
            ) {
                Ok(sc) => sc,
                Err(_) => continue,
            };

            for sc in symbol_changes {
                let dedup_key = format!("{}\0{}", sc.symbol, entry.path);
                if !seen_keys.insert(dedup_key) {
                    continue;
                }

                let change_type = match sc.change {
                    ChangeKind::Added => "added",
                    ChangeKind::Deleted => "deleted",
                    ChangeKind::SignatureChanged => "signature_changed",
                    ChangeKind::BodyChanged => "body_changed",
                    ChangeKind::CosmeticChanged => "cosmetic_changed",
                    ChangeKind::Renamed => "renamed",
                    ChangeKind::Moved => "moved",
                };

                changes.push(EntityChangeRow {
                    qualified_name: sc.symbol,
                    kind: sc.kind,
                    file_path: String::from(entry.path.as_str()),
                    change_type: change_type.to_string(),
                    old_qualified_name: sc.from_symbol,
                    old_file_path: sc.from_file,
                });
            }
        }

        total_changes += changes.len();
        db.insert_entity_commit_with_changes(
            &commit.hash,
            commit.timestamp,
            &commit.author,
            &changes,
        )?;
    }

    Ok(total_changes)
}

/// Resolve pending cross-file references and rebuild import edges — the tier of
/// derived data that graph and symbol queries read directly. Shared by the full
/// parse (via [`post_parse_sequence`]) and the query-path incremental refresh
/// ([`parse_incremental`], sutra/363). Returns the resolution counts.
fn resolve_references(db: &Db, workspace_root: &Path) -> Result<(i64, i64, i64)> {
    // Query resolution work from DB — includes freshly-parsed files,
    // dependents of deleted symbols, and orphans from interrupted parses.
    log_phase_rss("post_parse:start");
    let resolution_ids = db.files_needing_resolution()?;
    let resolution_set: HashSet<i64> = resolution_ids.into_iter().collect();

    let all_db_symbols = db.all_symbols_summary()?;
    let symbol_index = resolver::SymbolIndex::build(&all_db_symbols);
    log_phase_rss("post_parse:symbols_loaded");
    let mut resolved_count: i64 = 0;
    let mut unresolved_count: i64 = 0;
    let mut skipped_count: i64 = 0;
    let resolution_total = resolution_set.len();
    let resolution_started = Instant::now();
    let mut last_progress = Instant::now();
    let mut bulk = BulkBatch::new(db, resolution_total >= BULK_MODE_THRESHOLD);
    for (done, &file_id) in resolution_set.iter().enumerate() {
        bulk.file_start()?;
        let (resolved, unresolved, skipped) = resolve_file_refs(db, file_id, &symbol_index)?;
        bulk.file_done()?;
        resolved_count += resolved;
        unresolved_count += unresolved;
        skipped_count += skipped;
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            last_progress = Instant::now();
            info!(
                files_resolved = done + 1,
                files_total = resolution_total,
                resolved = resolved_count,
                unresolved = unresolved_count,
                elapsed_s = resolution_started.elapsed().as_secs(),
                "resolution progress"
            );
        }
    }
    bulk.commit()?;

    let dart_resolved = crate::dart_packages::resolve_dart_imports(db, workspace_root)?;
    if dart_resolved > 0 {
        info!(count = dart_resolved, "resolved Dart import edges");
    }

    let rust_resolved = crate::rust_imports::resolve_rust_imports(db, workspace_root)?;
    if rust_resolved > 0 {
        info!(count = rust_resolved, "resolved Rust import edges");
    }

    let c_resolved = crate::c_imports::resolve_c_imports(db, workspace_root)?;
    if c_resolved > 0 {
        info!(count = c_resolved, "resolved C import edges");
    }

    let python_resolved = crate::python_imports::resolve_python_imports(db, workspace_root)?;
    if python_resolved > 0 {
        info!(count = python_resolved, "resolved Python import edges");
    }

    let js_resolved = crate::js_imports::resolve_js_imports(db, workspace_root)?;
    if js_resolved > 0 {
        info!(count = js_resolved, "resolved JS/TS import edges");
    }

    log_phase_rss("post_parse:refs_resolved");
    Ok((resolved_count, unresolved_count, skipped_count))
}

/// Query-path incremental refresh (sutra/363): reparse only the drifted files
/// and drop the removed ones, then re-resolve references. Writes exactly the
/// tier that graph and symbol queries read — files, symbols, refs, and import
/// edges — and deliberately skips the expensive derived tiers (pagerank, file
/// rollups, components, semantic anchors, HRR vectors, health findings,
/// cochange, and pattern families), which remain the job of an explicit
/// `reparse`. The caller holds the per-workspace parse lock across this call.
///
/// No snapshot is recorded: staleness is content-based (per-file fingerprints,
/// updated by `replace_file_data`), so the next drift probe reads clean without
/// a new snapshot, and `set_derived_complete` is deliberately NOT called — the
/// derived tier really is behind until a full reparse catches it up.
pub fn parse_incremental(
    workspace: &WorkspaceEntry,
    db: &Db,
    config: &Config,
    registry: &LanguageRegistry,
    drift: &crate::freshness::WorkspaceDrift,
) -> Result<ParseSnapshot> {
    let _flock = acquire_parse_flock(config, &workspace.id)?;
    let start = Instant::now();
    let mut pool = ParserPool::new(Duration::from_millis(config.parse_timeout_ms));

    // Drop files the walk no longer yields (deleted or newly ignored).
    let mut removed = 0i64;
    for rel in &drift.removed {
        if let Some(f) = db.file_by_path(rel)? {
            db.delete_file_cascade(f.id)?;
            removed += 1;
        }
    }

    let mut files_parsed: i64 = 0;
    let mut symbols_extracted: i64 = 0;
    let mut refs_extracted: i64 = 0;
    let mut parse_errors: i64 = 0;
    // The query path only ever reaches non-frozen workspaces; never trust mtime
    // (trust_mtime=false), so a mtime-preserving edit is still caught by the
    // content-hash check inside parse_single_file.
    for rel in drift.changed.iter().chain(&drift.added) {
        let full = workspace.root.join(rel);
        // Content-drift path: these files are already known-changed, and an
        // extractor change is healed by the full parse (parse_workspace), not
        // the query hot path — so never force here (force_reparse=false).
        if let Some(result) = parse_single_file(
            db,
            &full,
            &workspace.root,
            registry,
            &mut pool,
            false,
            false,
        )? {
            parse_errors += result.parse_errors;
            if result.file_id != 0 {
                files_parsed += 1;
                symbols_extracted += result.symbols_extracted;
                refs_extracted += result.refs_extracted;
            }
        }
    }

    let (resolved_count, unresolved_count, skipped_count) =
        resolve_references(db, &workspace.root)?;

    let duration_ms = start.elapsed().as_millis() as i64;
    info!(
        workspace = %workspace.id,
        changed = drift.changed.len(),
        added = drift.added.len(),
        removed,
        files_parsed,
        symbols_extracted,
        duration_ms,
        "query-path incremental reparse"
    );

    Ok(ParseSnapshot {
        files_walked: (drift.changed.len() + drift.added.len()) as i64,
        files_parsed,
        symbols_extracted,
        refs_extracted,
        parse_errors,
        duration_ms,
        resolved_count,
        unresolved_count,
        skipped_count,
    })
}

fn post_parse_sequence(
    db: &Db,
    workspace_root: &Path,
    boundary_multipliers: &HashMap<String, f64>,
    registry: &LanguageRegistry,
    health_session: &crate::health::refresh::HealthSession<'_>,
) -> Result<(i64, i64, i64)> {
    let (resolved_count, unresolved_count, skipped_count) = resolve_references(db, workspace_root)?;

    let files = db.all_files()?;
    if !files.is_empty() {
        let gd = graph::GraphData::load(db)?;
        log_phase_rss("post_parse:graph_loaded");
        let adjacency = graph::build_file_adjacency(&files, &gd);
        graph::compute_rollups_with_adjacency(db, &files, &adjacency)?;
        graph::compute_pagerank_with_adjacency(db, &files, &adjacency, &gd)?;
        log_phase_rss("post_parse:pagerank_done");

        // Ingest commit-file history against the pinned HEAD and the absolute
        // day-quantized cutoff the health contract requires (sutra/415), through
        // the shared refresh core so full parse and on-demand health select
        // history identically. This also resolves the git-availability axis
        // (sutra/408: a transient failure is NoHistory-worst-cased, never
        // structural absence) and the churn map semantic anchors consume.
        let health_day = crate::health::probe::utc_day(chrono::Utc::now().timestamp());
        let health_window = crate::health::refresh::window_days(workspace_root)?;
        let graph_stamp = crate::health::probe::probe_graph_stamp(db)?;
        let ingestion = crate::health::refresh::ingest_history(
            health_session,
            db,
            workspace_root,
            health_day,
            health_window,
            graph_stamp.generation,
            graph_stamp.indexed_paths,
        )?;
        let churn_map = ingestion.churn;
        let health_history = ingestion.observation;
        log_phase_rss("post_parse:cochange_done");

        match entity_change_walk(db, workspace_root, 500) {
            Ok(count) if count > 0 => info!(count, "indexed entity changes"),
            Ok(_) => {}
            Err(e) => warn!("entity change walk failed: {e}"),
        }
        log_phase_rss("post_parse:entity_walk_done");

        info!(files = files.len(), "discovering components");
        let component_count =
            components::discover_components(db, &files, &gd, workspace_root, boundary_multipliers)?;
        log_phase_rss("post_parse:components_done");
        if component_count > 0 {
            info!(component_count, "discovered components");
            let anchor_count = components::compute_semantic_anchors(db, &gd, &churn_map)?;
            if anchor_count > 0 {
                info!(anchor_count, "computed semantic anchors");
            }
        }
        log_phase_rss("post_parse:anchors_done");

        let alias_count = crate::vocabulary::sync_aliases(db, workspace_root)?;
        if alias_count > 0 {
            info!(alias_count, "synced vocabulary aliases");
        }
        log_phase_rss("post_parse:aliases_done");

        info!("similarity: computing HRR vectors");
        let (hrr_count, hrr_changed) = crate::similarity::compute_hrr_vectors(db, workspace_root)?;
        info!(count = hrr_count, "similarity: HRR vectors done");
        log_phase_rss("post_parse:hrr_done");

        let mut loaded_rules = crate::rules::load_rules(workspace_root)?;
        let (all_constraints, _parse_errors) = loaded_rules.all_constraints();
        let ratchet_count =
            crate::constraints::register_ratcheted_constraints(db, &all_constraints)?;
        if ratchet_count > 0 {
            info!(count = ratchet_count, "registered ratcheted constraints");
        }

        let conv_outcome = crate::conventions::pipeline::rebuild(db, registry, workspace_root)?;
        if conv_outcome.convention_count > 0 {
            info!(count = conv_outcome.convention_count, "rebuilt conventions");
        }

        // Publish an immutable health run through the shared refresh core so full
        // parse and on-demand agree on findings/completeness (sutra/415, AC3).
        // Scoring reads the published run (sutra/416), never the live tables. Nothing bumps data_generation between the
        // graph probe above and here, so the run publishes at that generation.
        match crate::health::refresh::publish_run(
            health_session,
            db,
            workspace_root,
            graph_stamp,
            health_history,
        )? {
            crate::health::refresh::RefreshResult::Published(_) => {}
            other => {
                warn!(?other, "health run not published on full parse");
            }
        }
        log_phase_rss("post_parse:health_done");

        if hrr_changed {
            info!("similarity: detecting pattern families");
            let family_count = crate::similarity::compute_pattern_families(db)?;
            info!(count = family_count, "similarity: pattern families done");
            log_phase_rss("post_parse:pattern_families_done");
        }
    }

    Ok((resolved_count, unresolved_count, skipped_count))
}

/// Per-parse facts recorded on every checkpoint.
struct CheckpointMeta {
    head_commit: Option<String>,
    timestamp: Option<String>,
    files_parsed: i64,
    symbols_extracted: i64,
    refs_extracted: i64,
    parse_errors: i64,
    duration_ms: i64,
}

/// Parse-derived workspace metrics (not health).
struct ParseAggregates {
    total_complexity: i64,
    dead_symbol_count: i64,
    hotspot_count: i64,
    pattern_family_count: i64,
}

fn record_snapshot(db: &Db, workspace_root: &Path, meta: CheckpointMeta) -> Result<()> {
    let aggregates = compute_parse_aggregates(db)?;
    write_checkpoint(db, workspace_root, meta, aggregates)
}

/// A no-change parse copies the parse-derived aggregates forward (the source is
/// unchanged), but always rescores health from the current validated run: health
/// inputs (history day/HEAD, owners, waivers) move independently of source bytes,
/// so copying the previous checkpoint's health rows could present evidence from
/// an older run as current (health-evidence-contract.md § Publication and
/// consumers: copy-forward only for unchanged health inputs and score basis).
fn record_unchanged_snapshot(db: &Db, workspace_root: &Path, meta: CheckpointMeta) -> Result<()> {
    let aggregates = match db.latest_snapshots(1)?.into_iter().next() {
        Some(previous) => ParseAggregates {
            total_complexity: previous.total_complexity,
            dead_symbol_count: previous.dead_symbol_count,
            hotspot_count: previous.hotspot_count,
            pattern_family_count: previous.pattern_family_count,
        },
        None => compute_parse_aggregates(db)?,
    };
    write_checkpoint(db, workspace_root, meta, aggregates)
}

fn write_checkpoint(
    db: &Db,
    workspace_root: &Path,
    meta: CheckpointMeta,
    aggregates: ParseAggregates,
) -> Result<()> {
    let health = compute_snapshot_health(db, workspace_root)?;
    db.insert_snapshot_atomic(
        &SnapshotParams {
            files_parsed: meta.files_parsed,
            symbols_extracted: meta.symbols_extracted,
            refs_extracted: meta.refs_extracted,
            parse_errors: meta.parse_errors,
            duration_ms: meta.duration_ms,
            total_complexity: aggregates.total_complexity,
            dead_symbol_count: aggregates.dead_symbol_count,
            hotspot_count: aggregates.hotspot_count,
            health_score: health.health_score,
            pattern_family_count: aggregates.pattern_family_count,
            head_commit: meta.head_commit,
            timestamp: meta.timestamp,
            health_run_id: health.run_id,
        },
        &health.file_scores,
        &health.component_scores,
    )?;
    Ok(())
}

/// Recursively walk `root` and collect files with matching extensions, also
/// reporting whether the traversal completed without per-entry errors. A `false`
/// completeness flag means the walk *under-yielded* — an unreadable subtree, a
/// transient I/O failure — so the returned path set is only a lower bound.
/// Callers that derive *deletions* from "indexed but not walked" MUST NOT do so
/// on an incomplete walk: the shortfall is missing paths, not deleted files, and
/// treating it as deletions cascade-deletes live rows (sutra/379).
/// Skips hidden dirs and known build output directories.
pub(crate) fn walk_source_files_checked(
    root: &Path,
    allowed_extensions: &[&str],
) -> (Vec<std::path::PathBuf>, bool) {
    let mut result = Vec::new();
    let mut complete = true;

    for entry in workspace_walker(root).build() {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!(error = %e, "could not read workspace entry");
                complete = false;
                continue;
            }
        };
        let path = entry.path();
        if entry.file_type().is_some_and(|ft| ft.is_file())
            && let Some(ext) = path.extension().and_then(|e| e.to_str())
            && allowed_extensions.contains(&ext)
        {
            result.push(path.to_path_buf());
        }
    }

    // Sort for deterministic ordering.
    result.sort();
    (result, complete)
}

/// Recursively walk `root` and collect files with matching extensions.
/// Skips hidden dirs and known build output directories. Discards the walk's
/// completeness signal — use [`walk_source_files_checked`] when the caller
/// derives deletions from what the walk did not yield.
pub(crate) fn walk_source_files(
    root: &Path,
    allowed_extensions: &[&str],
) -> Vec<std::path::PathBuf> {
    walk_source_files_checked(root, allowed_extensions).0
}

struct SnapshotHealthData {
    health_score: f64,
    run_id: Option<i64>,
    file_scores: Vec<SnapshotFileRow>,
    component_scores: Vec<SnapshotComponentRow>,
}

fn compute_parse_aggregates(db: &Db) -> Result<ParseAggregates> {
    let files = db.all_files()?;
    let complexity = db.complexity_by_file()?;
    let total_complexity: i64 = complexity
        .values()
        .map(|&(_, avg_cog)| avg_cog as i64)
        .sum();
    let dead_symbol_count = db.find_dead_symbols(false, None)?.len() as i64;
    let hotspot_count = files
        .iter()
        .filter(|f| {
            let (_, avg_cog) = complexity.get(&f.id).copied().unwrap_or((0, 0.0));
            f.blast_radius >= 5 && avg_cog >= 5.0
        })
        .count() as i64;
    Ok(ParseAggregates {
        total_complexity,
        dead_symbol_count,
        hotspot_count,
        pattern_family_count: db.pattern_family_count()?,
    })
}

/// The verdict on the current health run for a checkpoint. A failed probe
/// records the checkpoint as stale-partial rather than dropping it.
fn snapshot_verdict(db: &Db, workspace_root: &Path) -> crate::health::assess::RunVerdict {
    use crate::health::evidence::{InputFailure, MissingReason};
    match crate::health::refresh::current_run_validity(
        db,
        workspace_root,
        chrono::Utc::now().timestamp(),
    ) {
        Ok(v) => v,
        Err(e) => {
            warn!("snapshot: health validity probe failed: {e}");
            crate::health::assess::RunVerdict::stale(MissingReason::Failed(
                InputFailure::ProbeFailed,
            ))
        }
    }
}

fn compute_snapshot_health(db: &Db, workspace_root: &Path) -> Result<SnapshotHealthData> {
    use crate::health::assess::{self, PersistentEvidence};
    use crate::health::scoring::ScoreValue;

    // Score from the validated current run (sutra/416): the checkpoint records
    // exactly what the evidence supports — measured scores, or conservative
    // bounds with the missing producers — plus the basis they were scored under
    // and the run they came from.
    let evidence = PersistentEvidence::load(db, snapshot_verdict(db, workspace_root))?;
    // Component clustering is only rebuilt by a full parse; a stale grouping is
    // recorded partial, never measured (a probe failure reads as stale).
    let membership_current = crate::components::membership_current(db, workspace_root)
        .unwrap_or_else(|e| {
            warn!("snapshot: component membership probe failed: {e}");
            false
        });
    let workspace = assess::score_workspace(db, &evidence, membership_current)?;

    let mut file_scores = Vec::with_capacity(workspace.files.len());
    let mut health_sum = 0.0;
    for sf in &workspace.files {
        let cat_totals: HashMap<&str, f64> = sf
            .score
            .categories
            .iter()
            .map(|c| (c.category.as_str(), c.pessimistic))
            .collect();
        let category_scores = serde_json::to_string(&cat_totals).unwrap_or_else(|_| "{}".into());
        let (completeness, score_upper) = match sf.score.value {
            ScoreValue::Measured(_) => (SnapshotCompleteness::Complete, None),
            ScoreValue::Partial { upper, .. } => (SnapshotCompleteness::Partial, Some(upper)),
        };
        let score = sf.score.value.lower();
        health_sum += score;
        file_scores.push(SnapshotFileRow {
            file_id: sf.evidence.file_id,
            file_path: sf.evidence.path.to_string(),
            score,
            category_scores,
            completeness,
            missing_biomarkers: sf.score.missing_names(),
            score_upper,
            score_basis: Some(sf.evidence.basis.to_hex()),
        });
    }

    let health_score = if file_scores.is_empty() {
        10.0
    } else {
        health_sum / file_scores.len() as f64
    };

    let component_scores = workspace
        .components
        .into_iter()
        .map(|cs| SnapshotComponentRow {
            score: cs.value.lower(),
            completeness: if cs.value.is_measured() {
                SnapshotCompleteness::Complete
            } else {
                SnapshotCompleteness::Partial
            },
            score_basis: Some(cs.basis.to_hex()),
            component_id: cs.component_id,
            component_name: cs.component_name,
            member_count: cs.member_count as i64,
            total_nloc: cs.total_nloc,
        })
        .collect();

    Ok(SnapshotHealthData {
        health_score,
        run_id: evidence.run_id.map(|r| r.0),
        file_scores,
        component_scores,
    })
}

fn parse_symbol_kind(s: &str) -> parser::SymbolKind {
    s.parse().unwrap_or_else(|_| {
        warn!(kind = s, "unknown symbol kind, defaulting to function");
        parser::SymbolKind::Function
    })
}

fn parse_ref_context_kind(s: &str) -> parser::RefContextKind {
    s.parse().unwrap_or_else(|_| {
        warn!(kind = s, "unknown ref context kind, defaulting to other");
        parser::RefContextKind::Other
    })
}

#[cfg(test)]
mod parse_coordinator_tests {
    use super::ParseCoordinator;

    // sutra/300: the parse lock is now held by DD-backed evaluation too
    // (sutra/298), so `parsing_in_progress` must NOT be derived from the raw
    // lock — an evaluation lock holder is not a parse. `is_parsing` reads a
    // separate flag set only by `mark_parsing`.
    #[test]
    fn mark_parsing_toggles_is_parsing_and_clears_on_drop() {
        let coord = ParseCoordinator::new();
        assert!(!coord.is_parsing("ws"), "no parse marked yet");
        {
            let _mark = coord.mark_parsing("ws");
            assert!(coord.is_parsing("ws"), "mark held → parsing");
        }
        assert!(!coord.is_parsing("ws"), "mark dropped → not parsing");
    }

    // The exact regression Codex flagged: holding the parse lock the way DD
    // evaluation does (no mark) must leave `is_parsing` false, so freshness
    // never reports an evaluator as a parser.
    #[test]
    fn holding_lock_without_marking_is_not_reported_as_parsing() {
        let coord = ParseCoordinator::new();
        let lock = coord.lock_for("ws");
        let _guard = lock.try_lock().expect("uncontended");
        assert!(
            !coord.is_parsing("ws"),
            "evaluation-style lock hold must not look like a parse"
        );
    }

    // sutra/380: the query-path refresh moves the parse lock guard and mark INTO
    // the spawn_blocking task. spawn_blocking tasks are not cancelled when the
    // awaiting future is dropped (an HTTP client disconnect / request
    // cancellation), so the lock must stay held for the whole write — otherwise a
    // concurrent refresh or DD read could enter and read a half-written index.
    // This models that path: abandon the await, assert a second acquirer cannot
    // enter until the detached task finishes.
    #[tokio::test]
    async fn abandoned_await_keeps_lock_and_mark_until_blocking_task_finishes() {
        use std::sync::mpsc;

        let coord = ParseCoordinator::new();
        let lock = coord.lock_for("ws");
        let guard = lock.lock_owned().await;
        let mark = coord.mark_parsing("ws");

        let (started_tx, started_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();

        // Guard + mark move into the blocking task, exactly as the refresh path
        // does. They drop only when this closure returns.
        let handle = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let _mark = mark;
            started_tx.send(()).expect("signal start");
            release_rx.recv().expect("await release"); // stands in for the write
        });

        // Wait until the task holds the guard, then abandon the await — dropping
        // the JoinHandle detaches the blocking task; it is NOT cancelled.
        started_rx.recv().expect("task started");
        drop(handle);

        // The detached write still holds the lock + mark: a concurrent acquirer
        // keyed on the same workspace must contend, and freshness still sees a
        // parse in progress.
        assert!(
            coord.lock_for("ws").try_lock().is_err(),
            "parse lock must stay held by the detached blocking task"
        );
        assert!(coord.is_parsing("ws"), "parse mark must stay set mid-write");

        // Let the write finish. `_mark` drops before `_guard` (reverse decl
        // order), so once the lock is reacquirable the mark is already cleared.
        release_tx.send(()).expect("release the write");
        let reacquired = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            coord.lock_for("ws").lock_owned(),
        )
        .await;
        assert!(
            reacquired.is_ok(),
            "lock never released after the detached task finished"
        );
        assert!(
            !coord.is_parsing("ws"),
            "mark cleared once the detached task dropped it"
        );
    }

    // The lock is still the serialization primitive that excludes a concurrent
    // parse/evaluation on the same workspace.
    #[test]
    fn lock_for_serializes_same_workspace() {
        let coord = ParseCoordinator::new();
        let held = coord.lock_for("ws");
        let _guard = held.try_lock().expect("uncontended first acquire");
        assert!(
            coord.lock_for("ws").try_lock().is_err(),
            "second acquire on same workspace must contend"
        );
        // A different workspace keys a different lock.
        assert!(
            coord.lock_for("other").try_lock().is_ok(),
            "distinct workspace must not contend"
        );
    }

    #[test]
    fn parsing_flag_is_keyed_per_workspace() {
        let coord = ParseCoordinator::new();
        let _mark = coord.mark_parsing("ws");
        assert!(coord.is_parsing("ws"));
        assert!(!coord.is_parsing("other"), "flag must not leak across ws");
    }
}

#[cfg(test)]
mod walk_tests {
    use super::walk_source_files;
    use std::fs;

    /// The walk must honor `.gitignore`: an ignored file and everything under an
    /// ignored directory are excluded, while tracked source is still found.
    #[test]
    fn walk_respects_gitignore() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(root.join(".gitignore"), "secret.rs\nignored_dir/\n").expect("gitignore");
        fs::write(root.join("keep.rs"), "fn keep() {}").expect("keep");
        fs::write(root.join("secret.rs"), "fn secret() {}").expect("secret");
        fs::create_dir(root.join("ignored_dir")).expect("mkdir");
        fs::write(root.join("ignored_dir").join("hidden.rs"), "fn h() {}").expect("hidden");

        let found: Vec<String> = walk_source_files(root, &["rs"])
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(
            found.contains(&"keep.rs".to_string()),
            "tracked file kept: {found:?}"
        );
        assert!(
            !found.contains(&"secret.rs".to_string()),
            "gitignored file excluded"
        );
        assert!(
            !found.iter().any(|p| p.contains("hidden.rs")),
            "contents of a gitignored dir excluded: {found:?}"
        );
    }

    /// `SKIP_DIRS` build outputs stay pruned even when not gitignored.
    #[test]
    fn walk_hard_skips_build_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        fs::write(root.join("keep.rs"), "fn keep() {}").expect("keep");
        fs::create_dir(root.join("target")).expect("mkdir");
        fs::write(root.join("target").join("gen.rs"), "fn gen() {}").expect("gen");

        let found = walk_source_files(root, &["rs"]);
        assert_eq!(found.len(), 1, "only keep.rs, target/ pruned: {found:?}");
        assert!(found[0].ends_with("keep.rs"));
    }
}
