//! Parse pipeline: walk workspace, parse files, resolve refs, compute rollups.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fs2::FileExt;
use parking_lot::Mutex;
use tracing::{info, warn};

use crate::components;
use crate::config::Config;
use crate::db::{
    Db, InsertImportParams, InsertRefParams, InsertSymbolParams, ResolvedRefRow,
    SnapshotComponentRow, SnapshotFileRow, SnapshotParams, SymbolEntry,
};
use crate::error::Result;
use crate::graph;
use crate::parser;
use crate::parser::adapter::{LanguageRegistry, ParserPool};
use crate::resolver;
use crate::workspace::WorkspaceEntry;

/// Shared per-workspace parse lock. MCP tool handlers acquire the lock before
/// parsing, preventing concurrent parses against the same SQLite database.
#[derive(Clone, Default)]
pub struct ParseCoordinator {
    locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl ParseCoordinator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lock_for(&self, ws_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks
            .lock()
            .entry(ws_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Returns `true` if a parse is currently running for this workspace.
    pub fn is_locked(&self, ws_id: &str) -> bool {
        let locks = self.locks.lock();
        match locks.get(ws_id) {
            Some(lock) => lock.try_lock().is_err(),
            None => false,
        }
    }
}

fn acquire_parse_flock(config: &Config, workspace_id: &str) -> Result<std::fs::File> {
    let lock_dir = config.db_dir.join(workspace_id);
    std::fs::create_dir_all(&lock_dir).map_err(|e| {
        crate::error::SutraError::Internal(format!(
            "could not create lock directory {}: {e}",
            lock_dir.display()
        ))
    })?;
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_dir.join("parse.lock"))
        .map_err(|e| {
            crate::error::SutraError::Internal(format!("could not open parse lock file: {e}"))
        })?;
    lock_file.lock_exclusive().map_err(|e| {
        crate::error::SutraError::Internal(format!("could not acquire parse lock: {e}"))
    })?;
    Ok(lock_file)
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

/// Maximum lines per file — files larger than this are skipped with a warning.
const MAX_LINES: usize = 100_000;

/// Directories to skip when walking the workspace.
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

struct FileParseResult {
    file_id: i64,
    symbols_extracted: i64,
    refs_extracted: i64,
    parse_errors: i64,
}

enum PostParseResult {
    Full {
        resolved_count: i64,
        unresolved_count: i64,
        skipped_count: i64,
    },
    NoChanges,
}

fn flatten_symbols_dfs<'a>(
    symbols: &'a [parser::ExtractedSymbol],
    parent_idx: Option<usize>,
    out: &mut Vec<InsertSymbolParams<'a>>,
    parents: &mut Vec<Option<usize>>,
) {
    for sym in symbols {
        let my_idx = out.len();
        out.push(InsertSymbolParams {
            file_id: 0, // filled by replace_file_data
            qualified_name: &sym.qualified_name,
            short_name: &sym.short_name,
            kind: sym.kind.as_str(),
            signature: sym.signature.as_deref(),
            signature_hash: sym.signature_hash.as_deref(),
            structural_hash: sym.structural_hash.as_deref(),
            visibility: sym.visibility.as_deref(),
            start_line: sym.start_line as i64,
            start_col: sym.start_col as i64,
            end_line: sym.end_line as i64,
            end_col: sym.end_col as i64,
            parent_symbol_id: None, // resolved via parent_indices
            docstring: sym.docstring.as_deref(),
            cyclomatic: sym.cyclomatic.map(|v| v as i64),
            cognitive: sym.cognitive.map(|v| v as i64),
            max_nesting: sym.max_nesting.map(|v| v as i64),
            flags: sym.flags as i64,
            language_attrs: sym.language_attrs.as_deref(),
        });
        parents.push(parent_idx);
        flatten_symbols_dfs(&sym.children, Some(my_idx), out, parents);
    }
}

fn flatten_symbols_for_insert(
    symbols: &[parser::ExtractedSymbol],
) -> (Vec<InsertSymbolParams<'_>>, Vec<Option<usize>>) {
    let mut out = Vec::new();
    let mut parents = Vec::new();
    flatten_symbols_dfs(symbols, None, &mut out, &mut parents);
    (out, parents)
}

fn parse_single_file(
    db: &Db,
    file_path: &Path,
    workspace_root: &Path,
    registry: &LanguageRegistry,
    pool: &mut ParserPool,
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

    let contents = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(path = %rel_path, error = %e, "could not read file");
            return Ok(Some(FileParseResult {
                file_id: 0,
                symbols_extracted: 0,
                refs_extracted: 0,
                parse_errors: 1,
            }));
        }
    };

    let line_count = contents.lines().count();
    if line_count > MAX_LINES {
        warn!(path = %rel_path, lines = line_count, max = MAX_LINES, "file exceeds line limit, skipping");
        return Ok(None);
    }

    let content_hash = blake3::hash(contents.as_bytes()).to_hex().to_string();

    let existing = db.file_by_path(&rel_path)?;
    if let Some(ref ex) = existing
        && ex.content_hash == content_hash
        && !db.file_has_null_language_attrs(ex.id)?
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
            }));
        }
    };

    let mut parse_errors: i64 = 0;
    if !parse_result.parsed_ok {
        parse_errors = 1;
    }

    let (flat_symbols, parent_indices) = flatten_symbols_for_insert(&parse_result.symbols);
    let import_params: Vec<InsertImportParams<'_>> = parse_result
        .imports
        .iter()
        .map(|imp| InsertImportParams {
            imported_path: &imp.raw_path,
            line: imp.line as i64,
            kind: imp.kind,
            alias: imp.alias.as_deref(),
        })
        .collect();
    let ref_params: Vec<InsertRefParams<'_>> = parse_result
        .references
        .iter()
        .map(|rf| InsertRefParams {
            unresolved_name: Some(&rf.name),
            line: rf.line as i64,
            col: rf.col as i64,
            context_kind: rf.context_kind.as_str(),
            resolved_local_target: rf.resolved_local_target.as_deref(),
            receiver: rf.receiver.as_deref(),
        })
        .collect();

    let (file_id, symbols_extracted) = db.replace_file_data(
        &rel_path,
        language,
        &content_hash,
        line_count as i64,
        parse_result.parsed_ok,
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
    }))
}

fn resolve_file_refs(
    db: &Db,
    file_id: i64,
    all_symbols: &[SymbolEntry],
    class_members: &resolver::ClassMembers<'_>,
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

    let extracted_refs: Vec<parser::ExtractedRef> = file_refs
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
        })
        .collect();

    let resolved = resolver::resolve_refs(
        &extracted_symbols,
        &extracted_refs,
        all_symbols,
        &extracted_imports,
        file_id,
        class_members,
    );

    let ref_rows: Vec<ResolvedRefRow<'_>> = resolved
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

    Ok((resolved_count, unresolved, skipped))
}

fn prune_deleted_files(db: &Db, workspace_root: &Path) -> usize {
    let files = match db.all_files() {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let mut count = 0;
    for file in &files {
        if !workspace_root.join(&*file.path).exists() {
            if let Err(e) = db.delete_file_cascade(file.id) {
                warn!(file = %file.path, "failed to prune deleted file: {e}");
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
    let _flock = acquire_parse_flock(config, &workspace.id)?;
    let head_commit = crate::git::head_commit_hash(&workspace.root);
    let parse_started_at = chrono::Utc::now().to_rfc3339();
    let start = Instant::now();
    let mut pool = ParserPool::new(Duration::from_millis(config.parse_timeout_ms));

    let allowed_extensions: Vec<&str> = registry.extensions_for_languages(&workspace.languages);

    // Prune indexed files that no longer exist on disk.
    let pruned = prune_deleted_files(db, &workspace.root);
    if pruned > 0 {
        info!(workspace = %workspace.id, pruned, "pruned deleted files from index");
    }

    let source_files = walk_source_files(&workspace.root, &allowed_extensions);
    info!(workspace = %workspace.id, files_found = source_files.len(), "walked workspace");

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

    let mut files_parsed: i64 = 0;
    let mut symbols_extracted: i64 = 0;
    let mut refs_extracted: i64 = 0;
    let mut parse_errors: i64 = 0;
    let inner = (|| -> Result<PostParseResult> {
        for file_path in &source_files {
            if cancel.load(Ordering::Relaxed) {
                return Err(crate::error::SutraError::Internal("parse cancelled".into()));
            }
            if let Some(result) =
                parse_single_file(db, file_path, &workspace.root, registry, &mut pool)?
            {
                parse_errors += result.parse_errors;
                if result.file_id != 0 {
                    files_parsed += 1;
                    symbols_extracted += result.symbols_extracted;
                    refs_extracted += result.refs_extracted;
                }
            }
        }

        if files_parsed == 0 && parse_errors == 0 && pruned == 0 && !db.has_pending_work()? {
            return Ok(PostParseResult::NoChanges);
        }

        let (resolved_count, unresolved_count, skipped_count) = post_parse_sequence(
            db,
            &workspace.root,
            &registry.boundary_multipliers(),
            registry,
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
    match &inner {
        Ok(PostParseResult::NoChanges) => {
            if let Err(e) = record_unchanged_snapshot(
                db,
                head_commit.clone(),
                Some(parse_started_at.clone()),
                files_parsed,
                symbols_extracted,
                refs_extracted,
                recorded_errors,
                duration_ms,
            ) {
                warn!(workspace = %workspace.id, "failed to record unchanged snapshot after parse: {e}");
            }
        }
        Ok(PostParseResult::Full { .. }) => {
            if let Err(e) = record_snapshot(
                db,
                head_commit.clone(),
                Some(parse_started_at.clone()),
                files_parsed,
                symbols_extracted,
                refs_extracted,
                recorded_errors,
                duration_ms,
            ) {
                warn!(workspace = %workspace.id, "failed to record snapshot after parse: {e}");
            } else if let Err(e) = db
                .get_data_generation()
                .and_then(|g| db.set_derived_complete(g))
            {
                warn!(workspace = %workspace.id, "failed to mark derived data complete: {e}");
            }
        }
        Err(_) => {
            if let Err(e) = record_snapshot(
                db,
                head_commit,
                Some(parse_started_at),
                files_parsed,
                symbols_extracted,
                refs_extracted,
                recorded_errors,
                duration_ms,
            ) {
                warn!(workspace = %workspace.id, "failed to record snapshot after parse: {e}");
            }
        }
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

fn post_parse_sequence(
    db: &Db,
    workspace_root: &Path,
    boundary_multipliers: &HashMap<String, f64>,
    registry: &LanguageRegistry,
) -> Result<(i64, i64, i64)> {
    // Query resolution work from DB — includes freshly-parsed files,
    // dependents of deleted symbols, and orphans from interrupted parses.
    let resolution_ids = db.files_needing_resolution()?;
    let resolution_set: HashSet<i64> = resolution_ids.into_iter().collect();

    let all_db_symbols = db.all_symbols_summary()?;
    let class_members = resolver::build_class_members(&all_db_symbols);
    let mut resolved_count: i64 = 0;
    let mut unresolved_count: i64 = 0;
    let mut skipped_count: i64 = 0;
    for &file_id in &resolution_set {
        let (resolved, unresolved, skipped) =
            resolve_file_refs(db, file_id, &all_db_symbols, &class_members)?;
        resolved_count += resolved;
        unresolved_count += unresolved;
        skipped_count += skipped;
    }

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

    let files = db.all_files()?;
    if !files.is_empty() {
        let gd = graph::GraphData::load(db)?;
        let adjacency = graph::build_file_adjacency(&files, &gd);
        let dirty_hint = if resolution_set.is_empty() {
            None
        } else {
            Some(&resolution_set)
        };
        graph::compute_rollups_with_adjacency(db, &files, &adjacency, dirty_hint)?;
        graph::compute_pagerank_with_adjacency(db, &files, &adjacency, &gd)?;

        let cochange_window = components::load_config(workspace_root)?
            .cochange_window_days
            .unwrap_or(90);
        let churn_map = match crate::git::git_commit_files(workspace_root, cochange_window) {
            Ok(commit_file_data) if !commit_file_data.is_empty() => {
                let churn = crate::git::churn_from_commit_files(&commit_file_data);
                let path_to_id: std::collections::HashMap<&str, i64> =
                    files.iter().map(|f| (&*f.path, f.id)).collect();
                let mut seen_hashes: std::collections::HashSet<&str> =
                    std::collections::HashSet::new();
                let mut commit_rows = Vec::new();
                for cf in &commit_file_data {
                    if seen_hashes.insert(cf.hash.as_str()) {
                        commit_rows.push(crate::db::CommitRow {
                            hash: cf.hash.clone(),
                            committed_at: cf.timestamp,
                            author: cf.author.clone(),
                        });
                    }
                }
                let db_pairs: Vec<(String, i64)> = commit_file_data
                    .iter()
                    .filter_map(|cf| {
                        path_to_id
                            .get(cf.path.as_str())
                            .map(|&id| (cf.hash.clone(), id))
                    })
                    .collect();
                db.replace_commit_files(&commit_rows, &db_pairs)?;
                churn
            }
            Ok(_) => {
                db.replace_commit_files(&[], &[])?;
                HashMap::new()
            }
            Err(e) => {
                warn!("git commit-file history unavailable: {e}");
                db.replace_commit_files(&[], &[])?;
                HashMap::new()
            }
        };

        match entity_change_walk(db, workspace_root, 500) {
            Ok(count) if count > 0 => info!(count, "indexed entity changes"),
            Ok(_) => {}
            Err(e) => warn!("entity change walk failed: {e}"),
        }

        let component_count =
            components::discover_components(db, &files, &gd, workspace_root, boundary_multipliers)?;
        if component_count > 0 {
            info!(component_count, "discovered components");
            let anchor_count = components::compute_semantic_anchors(db, &gd, &churn_map)?;
            if anchor_count > 0 {
                info!(anchor_count, "computed semantic anchors");
            }
        }

        let alias_count = crate::vocabulary::sync_aliases(db, workspace_root)?;
        if alias_count > 0 {
            info!(alias_count, "synced vocabulary aliases");
        }

        let (hrr_count, hrr_changed) = crate::similarity::compute_hrr_vectors(db, workspace_root)?;
        if hrr_count > 0 {
            info!(count = hrr_count, "computed HRR vectors");
        }

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

        let findings = crate::health::compute_all_health_findings(db, workspace_root)?;
        if !findings.is_empty() {
            info!(count = findings.len(), "computed health findings");
        }
        db.replace_health_findings(&findings)?;

        if hrr_changed {
            let family_count = crate::similarity::compute_pattern_families(db)?;
            if family_count > 0 {
                info!(count = family_count, "detected pattern families");
            }
        }
    }

    Ok((resolved_count, unresolved_count, skipped_count))
}

#[allow(clippy::too_many_arguments)]
fn record_snapshot(
    db: &Db,
    head_commit: Option<String>,
    timestamp: Option<String>,
    files_parsed: i64,
    symbols_extracted: i64,
    refs_extracted: i64,
    parse_errors: i64,
    duration_ms: i64,
) -> Result<()> {
    let health = compute_snapshot_health(db)?;
    db.insert_snapshot_atomic(
        &SnapshotParams {
            files_parsed,
            symbols_extracted,
            refs_extracted,
            parse_errors,
            duration_ms,
            total_complexity: health.total_complexity,
            dead_symbol_count: health.dead_symbol_count,
            hotspot_count: health.hotspot_count,
            health_score: health.health_score,
            pattern_family_count: health.pattern_family_count,
            head_commit,
            timestamp,
        },
        &health.file_scores,
        &health.component_scores,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn record_unchanged_snapshot(
    db: &Db,
    head_commit: Option<String>,
    timestamp: Option<String>,
    files_parsed: i64,
    symbols_extracted: i64,
    refs_extracted: i64,
    parse_errors: i64,
    duration_ms: i64,
) -> Result<()> {
    let Some(previous) = db.latest_snapshots(1)?.into_iter().next() else {
        return record_snapshot(
            db,
            head_commit,
            timestamp,
            files_parsed,
            symbols_extracted,
            refs_extracted,
            parse_errors,
            duration_ms,
        );
    };

    let file_scores = db.snapshot_file_scores(previous.id)?;
    let component_scores = db.snapshot_component_scores(previous.id)?;
    db.insert_snapshot_atomic(
        &SnapshotParams {
            files_parsed,
            symbols_extracted,
            refs_extracted,
            parse_errors,
            duration_ms,
            total_complexity: previous.total_complexity,
            dead_symbol_count: previous.dead_symbol_count,
            hotspot_count: previous.hotspot_count,
            health_score: previous.health_score,
            pattern_family_count: previous.pattern_family_count,
            head_commit,
            timestamp,
        },
        &file_scores,
        &component_scores,
    )?;
    Ok(())
}

/// Recursively walk `root` and collect files with matching extensions.
/// Skips hidden dirs and known build output directories.
fn walk_source_files(root: &Path, allowed_extensions: &[&str]) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(path = %dir.display(), error = %e, "could not read directory");
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name();
            let name = file_name.to_string_lossy();

            if path.is_dir() {
                // Skip hidden dirs and known build output.
                if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
                    continue;
                }
                stack.push(path);
            } else if path.is_file()
                && let Some(ext) = path.extension().and_then(|e| e.to_str())
                && allowed_extensions.contains(&ext)
            {
                result.push(path);
            }
        }
    }

    // Sort for deterministic ordering.
    result.sort();
    result
}

struct SnapshotHealthData {
    total_complexity: i64,
    dead_symbol_count: i64,
    hotspot_count: i64,
    health_score: f64,
    pattern_family_count: i64,
    file_scores: Vec<SnapshotFileRow>,
    component_scores: Vec<SnapshotComponentRow>,
}

fn compute_snapshot_health(db: &Db) -> Result<SnapshotHealthData> {
    use crate::health::scoring;

    let files = db.all_files()?;
    let complexity = db.complexity_by_file()?;

    let total_complexity: i64 = complexity
        .values()
        .map(|&(_, avg_cog)| avg_cog as i64)
        .sum();

    let dead_symbols = db.find_dead_symbols(false, None)?;
    let dead_symbol_count = dead_symbols.len() as i64;

    let mut hotspot_count: i64 = 0;
    for f in &files {
        let (_, avg_cog) = complexity.get(&f.id).copied().unwrap_or((0, 0.0));
        if f.blast_radius >= 5 && avg_cog >= 5.0 {
            hotspot_count += 1;
        }
    }

    let workspace = scoring::score_workspace(db, false)?;
    let file_score_map: HashMap<i64, &scoring::ScoredFile> = workspace
        .file_scores
        .iter()
        .map(|fs| (fs.file_id, fs))
        .collect();

    let mut file_scores = Vec::new();
    let mut health_sum = 0.0;

    for f in &files {
        let (score, cat_json) = match file_score_map.get(&f.id) {
            Some(sf) => {
                let cat_totals: HashMap<&str, f64> = sf
                    .category_totals
                    .iter()
                    .map(|(cat, &v)| (cat.as_str(), v))
                    .collect();
                let json = serde_json::to_string(&cat_totals).unwrap_or_else(|_| "{}".into());
                (sf.score, json)
            }
            None => (10.0, "{}".into()),
        };

        file_scores.push(SnapshotFileRow {
            file_id: f.id,
            file_path: f.path.to_string(),
            score,
            category_scores: cat_json,
        });
        health_sum += score;
    }

    let health_score = if files.is_empty() {
        10.0
    } else {
        health_sum / files.len() as f64
    };

    let component_scores = workspace
        .component_scores
        .into_iter()
        .map(|cs| SnapshotComponentRow {
            component_id: cs.component_id,
            component_name: cs.component_name,
            score: cs.score,
            member_count: cs.member_count as i64,
            total_nloc: cs.total_nloc,
        })
        .collect();

    let pattern_family_count = db.pattern_family_count()?;

    Ok(SnapshotHealthData {
        total_complexity,
        dead_symbol_count,
        hotspot_count,
        health_score,
        pattern_family_count,
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
