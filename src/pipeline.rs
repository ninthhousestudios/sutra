//! Parse pipeline: walk workspace, parse files, resolve refs, compute rollups.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::{info, warn};

use crate::components;
use crate::config::Config;
use crate::db::{Db, InsertSymbolParams, SnapshotParams};
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

/// Summary of a parse pipeline run.
#[derive(Debug, Clone)]
pub struct ParseSnapshot {
    pub files_walked: i64,
    pub files_parsed: i64,
    pub symbols_extracted: i64,
    pub refs_extracted: i64,
    pub parse_errors: i64,
    pub duration_ms: i64,
    pub unresolved_count: i64,
    /// Refs where resolution was skipped (Import, FieldAccess contexts).
    pub skipped_count: i64,
}

/// Maximum lines per file — files larger than this are skipped with a warning.
const MAX_LINES: usize = 100_000;

/// Directories to skip when walking the workspace.
const SKIP_DIRS: &[&str] = &[
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
    deleted_symbol_ids: Vec<i64>,
}

fn insert_symbols_dfs(
    db: &Db,
    file_id: i64,
    symbols: &[parser::ExtractedSymbol],
    parent_id: Option<i64>,
) -> Result<i64> {
    let mut count = 0;
    for sym in symbols {
        let id = db.insert_symbol(&InsertSymbolParams {
            file_id,
            qualified_name: &sym.qualified_name,
            short_name: &sym.short_name,
            kind: sym.kind.as_str(),
            signature: sym.signature.as_deref(),
            signature_hash: sym.signature_hash.as_deref(),
            visibility: sym.visibility.as_deref(),
            start_line: sym.start_line as i64,
            start_col: sym.start_col as i64,
            end_line: sym.end_line as i64,
            end_col: sym.end_col as i64,
            parent_symbol_id: parent_id,
            docstring: sym.docstring.as_deref(),
            cyclomatic: sym.cyclomatic.map(|v| v as i64),
            cognitive: sym.cognitive.map(|v| v as i64),
            flags: sym.flags as i64,
            language_attrs: sym.language_attrs.as_deref(),
        })?;
        count += 1;
        count += insert_symbols_dfs(db, file_id, &sym.children, Some(id))?;
    }
    Ok(count)
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
                deleted_symbol_ids: vec![],
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
    if let Some(ref ex) = existing {
        if ex.content_hash == content_hash
            && !db.file_has_null_language_attrs(ex.id)?
        {
            return Ok(None);
        }
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
                deleted_symbol_ids: vec![],
            }));
        }
    };

    let mut deleted_symbol_ids = Vec::new();
    if let Some(ex) = existing {
        let old_symbols = db.find_symbols_by_file(ex.id)?;
        for sym in &old_symbols {
            deleted_symbol_ids.push(sym.id);
        }
        db.delete_file_cascade(ex.id)?;
    }

    let mut parse_errors: i64 = 0;
    if !parse_result.parsed_ok {
        parse_errors = 1;
    }

    let file_id = db.upsert_file(
        &rel_path,
        language,
        &content_hash,
        line_count as i64,
        parse_result.parsed_ok,
    )?;

    let symbols_extracted = insert_symbols_dfs(db, file_id, &parse_result.symbols, None)?;

    for imp in &parse_result.imports {
        db.insert_import(file_id, &imp.raw_path, None, imp.line as i64)?;
    }

    let mut refs_extracted: i64 = 0;
    for rf in &parse_result.references {
        db.insert_ref(
            file_id,
            None,
            Some(&rf.name),
            rf.line as i64,
            rf.col as i64,
            rf.context_kind.as_str(),
        )?;
        refs_extracted += 1;
    }

    Ok(Some(FileParseResult {
        file_id,
        symbols_extracted,
        refs_extracted,
        parse_errors,
        deleted_symbol_ids,
    }))
}

fn resolve_file_refs(
    db: &Db,
    file_id: i64,
    all_symbols: &[(i64, String, String, String)],
) -> Result<(i64, i64)> {
    let file_symbols_rows = db.find_symbols_by_file(file_id)?;
    let file_refs = db.find_refs_in_file(file_id)?;
    let file_imports = db.imports_for_file(file_id)?;

    if file_refs.is_empty() {
        return Ok((0, 0));
    }

    let extracted_symbols: Vec<parser::ExtractedSymbol> = file_symbols_rows
        .iter()
        .map(|s| parser::ExtractedSymbol {
            qualified_name: s.qualified_name.clone(),
            short_name: s.short_name.clone(),
            kind: parse_symbol_kind(&s.kind),
            signature: s.signature.clone(),
            signature_hash: s.signature_hash.clone(),
            visibility: s.visibility.clone(),
            start_line: s.start_line as usize,
            start_col: s.start_col as usize,
            end_line: s.end_line as usize,
            end_col: s.end_col as usize,
            children: vec![],
            docstring: s.docstring.clone(),
            cyclomatic: s.cyclomatic.map(|v| v as u32),
            cognitive: s.cognitive.map(|v| v as u32),
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
        })
        .collect();

    let extracted_imports: Vec<parser::ExtractedImport> = file_imports
        .iter()
        .map(|i| parser::ExtractedImport {
            raw_path: i.imported_path.clone(),
            line: i.line as usize,
        })
        .collect();

    let resolved = resolver::resolve_refs(
        &extracted_symbols,
        &extracted_refs,
        all_symbols,
        &extracted_imports,
    );

    db.delete_refs_by_file(file_id)?;

    let mut unresolved: i64 = 0;
    let mut skipped: i64 = 0;
    for rr in &resolved {
        db.insert_ref(
            file_id,
            rr.target_symbol_id,
            rr.unresolved_name.as_deref(),
            rr.original.line as i64,
            rr.original.col as i64,
            rr.original.context_kind.as_str(),
        )?;
        if rr.skipped {
            skipped += 1;
        } else if rr.target_symbol_id.is_none() {
            unresolved += 1;
        }
    }

    Ok((unresolved, skipped))
}

fn prune_deleted_files(db: &Db, workspace_root: &Path) -> usize {
    let files = match db.all_files() {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let mut count = 0;
    for file in &files {
        if !workspace_root.join(&file.path).exists() {
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

    let mut files_parsed: i64 = 0;
    let mut symbols_extracted: i64 = 0;
    let mut refs_extracted: i64 = 0;
    let mut parse_errors: i64 = 0;
    let mut deleted_symbol_ids: Vec<i64> = Vec::new();
    let mut file_ids_needing_resolution: HashSet<i64> = HashSet::new();

    let inner = (|| -> Result<(i64, i64)> {
        for file_path in &source_files {
            if cancel.load(Ordering::Relaxed) {
                return Err(crate::error::SutraError::Internal("parse cancelled".into()));
            }
            if let Some(result) = parse_single_file(db, file_path, &workspace.root, registry, &mut pool)? {
                parse_errors += result.parse_errors;
                deleted_symbol_ids.extend(result.deleted_symbol_ids);
                if result.file_id != 0 {
                    files_parsed += 1;
                    symbols_extracted += result.symbols_extracted;
                    refs_extracted += result.refs_extracted;
                    file_ids_needing_resolution.insert(result.file_id);
                }
            }
        }
        post_parse_sequence(db, &deleted_symbol_ids, &mut file_ids_needing_resolution, &workspace.root, &registry.boundary_multipliers())
    })();

    let duration_ms = start.elapsed().as_millis() as i64;
    let recorded_errors = match &inner {
        Ok(_) => parse_errors,
        Err(_) => parse_errors.max(1),
    };
    if let Err(e) = record_snapshot(
        db,
        files_parsed,
        symbols_extracted,
        refs_extracted,
        recorded_errors,
        duration_ms,
    ) {
        warn!(workspace = %workspace.id, "failed to record snapshot after parse: {e}");
    }

    let (unresolved_count, skipped_count) = inner?;

    Ok(ParseSnapshot {
        files_walked: source_files.len() as i64,
        files_parsed,
        symbols_extracted,
        refs_extracted,
        parse_errors,
        duration_ms,
        unresolved_count,
        skipped_count,
    })
}

pub fn parse_changed_files(
    workspace: &WorkspaceEntry,
    db: &Db,
    config: &Config,
    changed: &[PathBuf],
    deleted: &[PathBuf],
    cancel: &AtomicBool,
    registry: &LanguageRegistry,
) -> Result<ParseSnapshot> {
    let start = Instant::now();
    let mut pool = ParserPool::new(Duration::from_millis(config.parse_timeout_ms));

    let allowed_ext: HashSet<&str> = registry
        .extensions_for_languages(&workspace.languages)
        .into_iter()
        .collect();

    let mut files_parsed: i64 = 0;
    let mut symbols_extracted: i64 = 0;
    let mut refs_extracted: i64 = 0;
    let mut parse_errors: i64 = 0;
    let mut deleted_symbol_ids: Vec<i64> = Vec::new();
    let mut file_ids_needing_resolution: HashSet<i64> = HashSet::new();

    let inner = (|| -> Result<(i64, i64)> {
        for del_path in deleted {
            if cancel.load(Ordering::Relaxed) {
                return Err(crate::error::SutraError::Internal("parse cancelled".into()));
            }
            let rel_path = del_path
                .strip_prefix(&workspace.root)
                .unwrap_or(del_path)
                .to_string_lossy()
                .to_string();

            if let Some(existing) = db.file_by_path(&rel_path)? {
                let old_symbols = db.find_symbols_by_file(existing.id)?;
                for sym in &old_symbols {
                    deleted_symbol_ids.push(sym.id);
                }
                db.delete_file_cascade(existing.id)?;
            }
        }

        for file_path in changed {
            if cancel.load(Ordering::Relaxed) {
                return Err(crate::error::SutraError::Internal("parse cancelled".into()));
            }
            let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if !allowed_ext.contains(ext) {
                continue;
            }
            if let Some(result) = parse_single_file(db, file_path, &workspace.root, registry, &mut pool)? {
                parse_errors += result.parse_errors;
                deleted_symbol_ids.extend(result.deleted_symbol_ids);
                if result.file_id != 0 {
                    files_parsed += 1;
                    symbols_extracted += result.symbols_extracted;
                    refs_extracted += result.refs_extracted;
                    file_ids_needing_resolution.insert(result.file_id);
                }
            }
        }

        post_parse_sequence(db, &deleted_symbol_ids, &mut file_ids_needing_resolution, &workspace.root, &registry.boundary_multipliers())
    })();

    let duration_ms = start.elapsed().as_millis() as i64;
    let recorded_errors = match &inner {
        Ok(_) => parse_errors,
        Err(_) => parse_errors.max(1),
    };
    if let Err(e) = record_snapshot(
        db,
        files_parsed,
        symbols_extracted,
        refs_extracted,
        recorded_errors,
        duration_ms,
    ) {
        warn!(workspace = %workspace.id, "failed to record snapshot after incremental parse: {e}");
    }

    let (unresolved_count, skipped_count) = inner?;

    Ok(ParseSnapshot {
        files_walked: changed.len() as i64,
        files_parsed,
        symbols_extracted,
        refs_extracted,
        parse_errors,
        duration_ms,
        unresolved_count,
        skipped_count,
    })
}

fn post_parse_sequence(
    db: &Db,
    deleted_symbol_ids: &[i64],
    file_ids_needing_resolution: &mut HashSet<i64>,
    workspace_root: &Path,
    boundary_multipliers: &HashMap<String, f64>,
) -> Result<(i64, i64)> {
    if !deleted_symbol_ids.is_empty() {
        let dirty_file_ids = db.find_files_referencing_symbols(deleted_symbol_ids)?;
        for fid in dirty_file_ids {
            file_ids_needing_resolution.insert(fid);
        }
    }

    let all_db_symbols = db.all_symbols_summary()?;
    let mut unresolved_count: i64 = 0;
    let mut skipped_count: i64 = 0;
    for &file_id in file_ids_needing_resolution.iter() {
        let (unresolved, skipped) = resolve_file_refs(db, file_id, &all_db_symbols)?;
        unresolved_count += unresolved;
        skipped_count += skipped;
    }

    let files = db.all_files()?;
    if !files.is_empty() {
        let adjacency = graph::build_file_adjacency(&files, db)?;
        graph::compute_rollups_with_adjacency(
            db,
            &files,
            &adjacency,
            Some(file_ids_needing_resolution),
        )?;
        graph::compute_pagerank_with_adjacency(db, &files, &adjacency)?;

        let cochange_window = components::load_config(workspace_root)?
            .cochange_window_days
            .unwrap_or(90);
        let commit_file_data = crate::git::git_commit_files(workspace_root, cochange_window)
            .unwrap_or_default();
        if !commit_file_data.is_empty() {
            let path_to_id: std::collections::HashMap<&str, i64> =
                files.iter().map(|f| (f.path.as_str(), f.id)).collect();
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
        }

        let component_count = components::discover_components(db, &files, workspace_root, boundary_multipliers)?;
        if component_count > 0 {
            info!(component_count, "discovered components");
            let anchor_count =
                components::compute_semantic_anchors(db, workspace_root)?;
            if anchor_count > 0 {
                info!(anchor_count, "computed semantic anchors");
            }
        }

        let alias_count = crate::vocabulary::sync_aliases(db, workspace_root)?;
        if alias_count > 0 {
            info!(alias_count, "synced vocabulary aliases");
        }
    }

    Ok((unresolved_count, skipped_count))
}

fn record_snapshot(
    db: &Db,
    files_parsed: i64,
    symbols_extracted: i64,
    refs_extracted: i64,
    parse_errors: i64,
    duration_ms: i64,
) -> Result<()> {
    let aggregates = compute_snapshot_aggregates(db)?;
    db.insert_snapshot(&SnapshotParams {
        files_parsed,
        symbols_extracted,
        refs_extracted,
        parse_errors,
        duration_ms,
        total_complexity: aggregates.total_complexity,
        dead_symbol_count: aggregates.dead_symbol_count,
        hotspot_count: aggregates.hotspot_count,
        health_score: aggregates.health_score,
    })?;
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

struct SnapshotAggregates {
    total_complexity: i64,
    dead_symbol_count: i64,
    hotspot_count: i64,
    health_score: i64,
}

fn compute_snapshot_aggregates(db: &Db) -> Result<SnapshotAggregates> {
    use crate::tools::file_health::compute_file_scores;

    let files = db.all_files()?;
    let complexity = db.complexity_by_file()?;
    let dead_ratios = db.dead_symbol_ratio_by_file()?;

    let total_complexity: i64 = complexity
        .values()
        .map(|&(_, avg_cog)| avg_cog as i64)
        .sum();

    let dead_symbols = db.find_dead_symbols(false, None)?;
    let dead_symbol_count = dead_symbols.len() as i64;

    let max_pr = files
        .iter()
        .filter_map(|f| f.pagerank)
        .fold(0.0_f64, f64::max)
        .max(0.001);

    let mut hotspot_count: i64 = 0;
    let mut health_sum: f64 = 0.0;

    for f in &files {
        let (max_cog, avg_cog) = complexity.get(&f.id).copied().unwrap_or((0, 0.0));
        let dead_ratio = dead_ratios.get(&f.id).copied().unwrap_or(0.0);
        let scores = compute_file_scores(f, max_cog, avg_cog, dead_ratio, max_pr);
        health_sum += scores.overall_health;

        if f.blast_radius >= 5 && avg_cog >= 5.0 {
            hotspot_count += 1;
        }
    }

    let health_score = if files.is_empty() {
        100
    } else {
        (health_sum / files.len() as f64) as i64
    };

    Ok(SnapshotAggregates {
        total_complexity,
        dead_symbol_count,
        hotspot_count,
        health_score,
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
