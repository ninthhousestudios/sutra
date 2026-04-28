//! Parse pipeline: walk workspace, parse files, resolve refs, compute rollups.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Instant;

use tracing::{info, warn};

use crate::config::Config;
use crate::db::Db;
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
}

/// Maximum lines per file — files larger than this are skipped with a warning.
const MAX_LINES: usize = 100_000;

/// Directories to skip when walking the workspace.
const SKIP_DIRS: &[&str] = &["target", "build", "node_modules"];

/// Map language name to file extensions.
fn extensions_for_language(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" => &["rs"],
        "dart" => &["dart"],
        _ => &[],
    }
}

/// Run the full parse pipeline for a workspace.
///
/// The `Db` is NOT Send/Sync, so we process files sequentially (v0.1).
pub async fn parse_workspace(
    workspace: &WorkspaceEntry,
    db: &Db,
    _config: &Config,
) -> Result<ParseSnapshot> {
    let start = Instant::now();

    // Collect allowed extensions from workspace languages.
    let allowed_extensions: Vec<&str> = workspace
        .languages
        .iter()
        .flat_map(|lang| extensions_for_language(lang))
        .copied()
        .collect();

    // Build a language lookup: extension → language name.
    let ext_to_lang: HashMap<&str, &str> = workspace
        .languages
        .iter()
        .flat_map(|lang| {
            extensions_for_language(lang)
                .iter()
                .map(move |ext| (*ext, lang.as_str()))
        })
        .collect();

    // --- Step 1: Walk workspace, collect source files ---
    let source_files = walk_source_files(&workspace.root, &allowed_extensions);
    info!(
        workspace = %workspace.id,
        files_found = source_files.len(),
        "walked workspace"
    );

    let mut files_parsed: i64 = 0;
    let mut symbols_extracted: i64 = 0;
    let mut refs_extracted: i64 = 0;
    let mut parse_errors: i64 = 0;
    let mut changed_file_ids: Vec<i64> = Vec::new();
    let mut deleted_symbol_ids: Vec<i64> = Vec::new();

    // Track which file_ids we've inserted symbols for (need resolution).
    let mut file_ids_needing_resolution: HashSet<i64> = HashSet::new();

    // --- Step 2: Parse each source file ---
    for file_path in &source_files {
        let rel_path = file_path
            .strip_prefix(&workspace.root)
            .unwrap_or(file_path.as_path())
            .to_string_lossy()
            .to_string();

        let ext = file_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        let language = match ext_to_lang.get(ext) {
            Some(lang) => *lang,
            None => continue,
        };

        // Read file contents.
        let contents = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %rel_path, error = %e, "could not read file");
                parse_errors += 1;
                continue;
            }
        };

        // Check line count cap.
        let line_count = contents.lines().count();
        if line_count > MAX_LINES {
            warn!(
                path = %rel_path,
                lines = line_count,
                max = MAX_LINES,
                "file exceeds line limit, skipping"
            );
            continue;
        }

        // Compute content hash.
        let content_hash = blake3::hash(contents.as_bytes()).to_hex().to_string();

        // Check if file is unchanged.
        if let Some(existing) = db.file_by_path(&rel_path)? {
            if existing.content_hash == content_hash {
                continue; // unchanged
            }

            // File changed — collect old symbol IDs before we delete them.
            let old_symbols = db.find_symbols_by_file(existing.id)?;
            for sym in &old_symbols {
                deleted_symbol_ids.push(sym.id);
            }

            // Delete old data (FK cascade handles symbols + refs).
            db.delete_file_cascade(existing.id)?;
        }

        // Parse.
        let parse_result = match parser::parse_file(&contents, language, &rel_path) {
            Ok(r) => r,
            Err(e) => {
                warn!(path = %rel_path, error = %e, "parse failed");
                parse_errors += 1;
                continue;
            }
        };

        if !parse_result.parsed_ok {
            parse_errors += 1;
        }

        // Upsert file row.
        let file_id = db.upsert_file(
            &rel_path,
            language,
            &content_hash,
            line_count as i64,
            parse_result.parsed_ok,
        )?;

        changed_file_ids.push(file_id);
        files_parsed += 1;

        // Insert symbols.
        for sym in &parse_result.symbols {
            db.insert_symbol(
                file_id,
                &sym.qualified_name,
                &sym.short_name,
                sym.kind.as_str(),
                sym.signature.as_deref(),
                sym.signature_hash.as_deref(),
                sym.visibility.as_deref(),
                sym.start_line as i64,
                sym.start_col as i64,
                sym.end_line as i64,
                sym.end_col as i64,
                None, // parent_symbol_id — not resolved in v0.1
                sym.docstring.as_deref(),
            )?;
            symbols_extracted += 1;
        }

        // Insert imports.
        for imp in &parse_result.imports {
            db.insert_import(file_id, &imp.raw_path, None, imp.line as i64)?;
        }

        // Insert refs (unresolved initially — we'll resolve in step 4).
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

        file_ids_needing_resolution.insert(file_id);
    }

    // --- Step 3: Cross-file dirty marking ---
    if !deleted_symbol_ids.is_empty() {
        let dirty_file_ids = db.find_files_referencing_symbols(&deleted_symbol_ids)?;
        for fid in dirty_file_ids {
            file_ids_needing_resolution.insert(fid);
        }
    }

    // --- Step 4: Resolve refs ---
    // Load all symbols from DB for resolution.
    let all_db_symbols = load_all_symbols(db)?;
    let mut unresolved_count: i64 = 0;

    for &file_id in &file_ids_needing_resolution {
        // Get file info for loading symbols/refs/imports.
        let file_symbols_rows = db.find_symbols_by_file(file_id)?;
        let file_refs = db.find_refs_in_file(file_id)?;
        let file_imports = db.imports_for_file(file_id)?;

        if file_refs.is_empty() {
            continue;
        }

        // Convert DB rows to parser types for the resolver.
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
                name: r
                    .unresolved_name
                    .clone()
                    .unwrap_or_default(),
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

        // Run resolver.
        let resolved = resolver::resolve_refs(
            &extracted_symbols,
            &extracted_refs,
            &all_db_symbols,
            &extracted_imports,
        );

        // Delete old refs for this file and re-insert with resolution data.
        // We already have the ref rows — delete them by file_id using a helper.
        delete_refs_for_file(db, file_id)?;

        for rr in &resolved {
            db.insert_ref(
                file_id,
                rr.target_symbol_id,
                rr.unresolved_name.as_deref(),
                rr.original.line as i64,
                rr.original.col as i64,
                rr.original.context_kind.as_str(),
            )?;
            if rr.target_symbol_id.is_none() {
                unresolved_count += 1;
            }
        }
    }

    // --- Step 5: Compute rollups ---
    compute_rollups(db)?;

    // --- Step 6: Record snapshot ---
    let duration_ms = start.elapsed().as_millis() as i64;
    db.insert_snapshot(
        files_parsed,
        symbols_extracted,
        refs_extracted,
        parse_errors,
        duration_ms,
    )?;

    Ok(ParseSnapshot {
        files_parsed,
        symbols_extracted,
        refs_extracted,
        parse_errors,
        duration_ms,
        unresolved_count,
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

/// Load all (id, qualified_name, short_name) tuples from the DB.
fn load_all_symbols(db: &Db) -> Result<Vec<(i64, String, String)>> {
    let files = db.all_files()?;
    let mut all = Vec::new();
    for f in &files {
        let syms = db.find_symbols_by_file(f.id)?;
        for s in syms {
            all.push((s.id, s.qualified_name, s.short_name));
        }
    }
    Ok(all)
}

/// Delete all refs for a given file_id.
///
/// The DB layer doesn't expose a direct "delete refs by file_id" method, but
/// since refs have ON DELETE CASCADE from files, we can't just delete the file.
/// Instead we use a raw approach via the existing API: we know the ref IDs from
/// `find_refs_in_file`. We'll add a helper method on Db for this.
fn delete_refs_for_file(db: &Db, file_id: i64) -> Result<()> {
    db.delete_refs_by_file(file_id)
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

/// Parse a kind string back into a SymbolKind.
fn parse_symbol_kind(s: &str) -> parser::SymbolKind {
    match s {
        "function" => parser::SymbolKind::Function,
        "method" => parser::SymbolKind::Method,
        "struct" => parser::SymbolKind::Struct,
        "enum" => parser::SymbolKind::Enum,
        "trait" => parser::SymbolKind::Trait,
        "impl" => parser::SymbolKind::Impl,
        "module" => parser::SymbolKind::Module,
        "const" => parser::SymbolKind::Const,
        "static" => parser::SymbolKind::Static,
        "type_alias" => parser::SymbolKind::TypeAlias,
        "macro" => parser::SymbolKind::Macro,
        "class" => parser::SymbolKind::Class,
        "mixin" => parser::SymbolKind::Mixin,
        "extension" => parser::SymbolKind::Extension,
        _ => parser::SymbolKind::Function, // fallback
    }
}

/// Parse a context_kind string back into a RefContextKind.
fn parse_ref_context_kind(s: &str) -> parser::RefContextKind {
    match s {
        "call" => parser::RefContextKind::Call,
        "type_use" => parser::RefContextKind::TypeUse,
        "import" => parser::RefContextKind::Import,
        "field_access" => parser::RefContextKind::FieldAccess,
        "pattern_bind" => parser::RefContextKind::PatternBind,
        _ => parser::RefContextKind::Other,
    }
}
