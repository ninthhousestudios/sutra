//! Parse pipeline: walk workspace, parse files, resolve refs, compute rollups.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Instant;

use tracing::{info, warn};

use crate::config::Config;
use crate::db::{Db, InsertSymbolParams};
use crate::error::Result;
use crate::parser;
use crate::resolver;
use crate::workspace::WorkspaceEntry;

/// Summary of a parse pipeline run.
#[derive(Debug, Clone)]
pub struct ParseSnapshot {
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
    "target", "build", "node_modules", ".git", "dist", "out", "vendor", "__pycache__", ".claude",
];

/// Map language name to file extensions.
fn extensions_for_language(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" => &["rs"],
        "dart" => &["dart"],
        _ => &[],
    }
}

struct FileParseResult {
    file_id: i64,
    symbols_extracted: i64,
    refs_extracted: i64,
    parse_errors: i64,
    deleted_symbol_ids: Vec<i64>,
}

fn parse_single_file(
    db: &Db,
    file_path: &Path,
    workspace_root: &Path,
    ext_to_lang: &HashMap<&str, &str>,
) -> Result<Option<FileParseResult>> {
    let rel_path = file_path
        .strip_prefix(workspace_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .to_string();

    let ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let language = match ext_to_lang.get(ext) {
        Some(lang) => *lang,
        None => return Ok(None),
    };

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

    let mut deleted_symbol_ids = Vec::new();
    if let Some(existing) = db.file_by_path(&rel_path)? {
        if existing.content_hash == content_hash {
            return Ok(None);
        }
        let old_symbols = db.find_symbols_by_file(existing.id)?;
        for sym in &old_symbols {
            deleted_symbol_ids.push(sym.id);
        }
        db.delete_file_cascade(existing.id)?;
    }

    let parse_result = match parser::parse_file(&contents, language, &rel_path) {
        Ok(r) => r,
        Err(e) => {
            warn!(path = %rel_path, error = %e, "parse failed");
            return Ok(Some(FileParseResult {
                file_id: 0,
                symbols_extracted: 0,
                refs_extracted: 0,
                parse_errors: 1,
                deleted_symbol_ids,
            }));
        }
    };

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

    let mut symbols_extracted: i64 = 0;
    for sym in &parse_result.symbols {
        db.insert_symbol(&InsertSymbolParams {
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
            parent_symbol_id: None,
            docstring: sym.docstring.as_deref(),
        })?;
        symbols_extracted += 1;
    }

    for imp in &parse_result.imports {
        db.insert_import(file_id, &imp.raw_path, None, imp.line as i64)?;
    }

    let mut refs_extracted: i64 = 0;
    for rf in &parse_result.references {
        db.insert_ref(file_id, None, Some(&rf.name), rf.line as i64, rf.col as i64, rf.context_kind.as_str())?;
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
            parent_qualified_name: None,
            docstring: s.docstring.clone(),
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

pub async fn parse_workspace(
    workspace: &WorkspaceEntry,
    db: &Db,
    config: &Config,
) -> Result<ParseSnapshot> {
    let start = Instant::now();

    let allowed_extensions: Vec<&str> = workspace
        .languages
        .iter()
        .flat_map(|lang| extensions_for_language(lang))
        .copied()
        .collect();

    let ext_to_lang: HashMap<&str, &str> = workspace
        .languages
        .iter()
        .flat_map(|lang| {
            extensions_for_language(lang)
                .iter()
                .map(move |ext| (*ext, lang.as_str()))
        })
        .collect();

    let source_files = walk_source_files(&workspace.root, &allowed_extensions);
    info!(workspace = %workspace.id, files_found = source_files.len(), "walked workspace");

    let mut files_parsed: i64 = 0;
    let mut symbols_extracted: i64 = 0;
    let mut refs_extracted: i64 = 0;
    let mut parse_errors: i64 = 0;
    let mut deleted_symbol_ids: Vec<i64> = Vec::new();
    let mut file_ids_needing_resolution: HashSet<i64> = HashSet::new();

    for file_path in &source_files {
        if let Some(result) = parse_single_file(db, file_path, &workspace.root, &ext_to_lang)? {
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

    if !deleted_symbol_ids.is_empty() {
        let dirty_file_ids = db.find_files_referencing_symbols(&deleted_symbol_ids)?;
        for fid in dirty_file_ids {
            file_ids_needing_resolution.insert(fid);
        }
    }

    let _ = config;

    let all_db_symbols = db.all_symbols_summary()?;
    let mut unresolved_count: i64 = 0;
    let mut skipped_count: i64 = 0;
    for &file_id in &file_ids_needing_resolution {
        let (unresolved, skipped) = resolve_file_refs(db, file_id, &all_db_symbols)?;
        unresolved_count += unresolved;
        skipped_count += skipped;
    }

    compute_rollups(db)?;
    compute_pagerank(db)?;

    let duration_ms = start.elapsed().as_millis() as i64;
    db.insert_snapshot(files_parsed, symbols_extracted, refs_extracted, parse_errors, duration_ms)?;

    Ok(ParseSnapshot {
        files_parsed,
        symbols_extracted,
        refs_extracted,
        parse_errors,
        duration_ms,
        unresolved_count,
        skipped_count,
    })
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

/// Compute fan_in_files and blast_radius for every file in the DB.
fn compute_rollups(db: &Db) -> Result<()> {
    let files = db.all_files()?;
    if files.is_empty() {
        return Ok(());
    }

    // Build adjacency: for each file, which other files reference its symbols?
    // ref.target_symbol_id → symbol.file_id gives us the "target file".
    // ref.file_id is the "source file" (the file containing the reference).

    // file_id → set of file_ids that reference symbols in it (fan_in).
    let mut fan_in_map: HashMap<i64, HashSet<i64>> = HashMap::new();
    // source_file_id → set of target_file_ids it references (outgoing edges).
    let mut outgoing: HashMap<i64, HashSet<i64>> = HashMap::new();

    for f in &files {
        fan_in_map.entry(f.id).or_default();
        outgoing.entry(f.id).or_default();
    }

    // Build a symbol_id → file_id lookup.
    let mut sym_to_file: HashMap<i64, i64> = HashMap::new();
    for f in &files {
        let syms = db.find_symbols_by_file(f.id)?;
        for s in syms {
            sym_to_file.insert(s.id, f.id);
        }
    }

    // Walk all refs to build edges.
    for f in &files {
        let refs = db.find_refs_in_file(f.id)?;
        for r in refs {
            if let Some(target_sym_id) = r.target_symbol_id
                && let Some(&target_file_id) = sym_to_file.get(&target_sym_id)
                && target_file_id != f.id
            {
                fan_in_map
                    .entry(target_file_id)
                    .or_default()
                    .insert(f.id);
                outgoing.entry(f.id).or_default().insert(target_file_id);
            }
        }
    }

    // Compute blast_radius via BFS, depth-capped at 3.
    for f in &files {
        let fan_in = fan_in_map.get(&f.id).map_or(0, |s| s.len()) as i64;

        // BFS from this file through fan_in edges (files that depend on this file,
        // then files that depend on those, etc.)
        let blast_radius = bfs_blast_radius(f.id, &fan_in_map, 3) as i64;

        db.update_rollups(f.id, fan_in, blast_radius)?;
    }

    Ok(())
}

/// BFS through the fan-in graph from `start`, depth-limited to `max_depth`.
/// Returns the count of distinct reachable files (excluding `start`).
fn bfs_blast_radius(
    start: i64,
    fan_in_map: &HashMap<i64, HashSet<i64>>,
    max_depth: usize,
) -> usize {
    let mut visited: HashSet<i64> = HashSet::new();
    visited.insert(start);
    let mut queue: VecDeque<(i64, usize)> = VecDeque::new();

    // Seed with files that directly reference symbols in `start`.
    if let Some(dependents) = fan_in_map.get(&start) {
        for &dep in dependents {
            if visited.insert(dep) {
                queue.push_back((dep, 1));
            }
        }
    }

    while let Some((file_id, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        if let Some(dependents) = fan_in_map.get(&file_id) {
            for &dep in dependents {
                if visited.insert(dep) {
                    queue.push_back((dep, depth + 1));
                }
            }
        }
    }

    // Exclude start itself.
    visited.len() - 1
}

/// Compute PageRank on the file dependency graph (ref edges + import edges),
/// then distribute file PageRank to symbols by incoming ref weight.
fn compute_pagerank(db: &Db) -> Result<()> {
    let files = db.all_files()?;
    if files.is_empty() {
        return Ok(());
    }

    let file_ids: Vec<i64> = files.iter().map(|f| f.id).collect();
    let n = file_ids.len();
    let id_to_idx: HashMap<i64, usize> = file_ids.iter().enumerate().map(|(i, &id)| (id, i)).collect();

    // Build outgoing adjacency from refs (same graph as compute_rollups).
    let mut out_edges: Vec<HashSet<usize>> = vec![HashSet::new(); n];
    let mut sym_to_file: HashMap<i64, i64> = HashMap::new();
    for f in &files {
        for s in db.find_symbols_by_file(f.id)? {
            sym_to_file.insert(s.id, f.id);
        }
    }
    for f in &files {
        let src_idx = id_to_idx[&f.id];
        for r in db.find_refs_in_file(f.id)? {
            if let Some(target_sym_id) = r.target_symbol_id
                && let Some(&target_file_id) = sym_to_file.get(&target_sym_id)
                && target_file_id != f.id
                && let Some(&dst_idx) = id_to_idx.get(&target_file_id)
            {
                out_edges[src_idx].insert(dst_idx);
            }
        }
    }

    for (src, dst) in db.import_edges()? {
        if let (Some(&si), Some(&di)) = (id_to_idx.get(&src), id_to_idx.get(&dst))
            && si != di
        {
            out_edges[si].insert(di);
        }
    }

    // Power iteration.
    const DAMPING: f64 = 0.85;
    const MAX_ITER: usize = 100;
    const EPSILON: f64 = 1e-6;

    let base = (1.0 - DAMPING) / n as f64;
    let mut rank = vec![1.0 / n as f64; n];

    for _ in 0..MAX_ITER {
        let mut next = vec![base; n];
        for (src, edges) in out_edges.iter().enumerate() {
            if edges.is_empty() {
                // Dangling node: distribute evenly.
                let share = DAMPING * rank[src] / n as f64;
                for r in next.iter_mut() {
                    *r += share;
                }
            } else {
                let share = DAMPING * rank[src] / edges.len() as f64;
                for &dst in edges {
                    next[dst] += share;
                }
            }
        }

        let max_delta = rank.iter().zip(next.iter()).map(|(a, b)| (a - b).abs()).fold(0.0_f64, f64::max);
        rank = next;
        if max_delta < EPSILON {
            break;
        }
    }

    // Write file pageranks.
    for (i, &file_id) in file_ids.iter().enumerate() {
        db.update_file_pagerank(file_id, rank[i])?;
    }

    // Symbol-level: distribute file PR to symbols by incoming ref count.
    let all_refs_to: HashMap<i64, usize> = {
        let mut counts: HashMap<i64, usize> = HashMap::new();
        for f in &files {
            for r in db.find_refs_in_file(f.id)? {
                if let Some(target) = r.target_symbol_id {
                    *counts.entry(target).or_default() += 1;
                }
            }
        }
        counts
    };

    for f in &files {
        let file_pr = rank[id_to_idx[&f.id]];
        let syms = db.find_symbols_by_file(f.id)?;
        let total_incoming: usize = syms.iter().map(|s| all_refs_to.get(&s.id).copied().unwrap_or(0)).sum();
        for s in &syms {
            let sym_refs = all_refs_to.get(&s.id).copied().unwrap_or(0);
            let sym_pr = if total_incoming > 0 {
                file_pr * sym_refs as f64 / total_incoming as f64
            } else {
                file_pr / syms.len().max(1) as f64
            };
            db.update_symbol_pagerank(s.id, sym_pr)?;
        }
    }

    Ok(())
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
