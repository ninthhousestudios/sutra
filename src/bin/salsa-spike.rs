use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use salsa::Setter;
use sutra::parser;

const SEP: &str = "============================================================";

// -------------------------------------------------------------------------
// Salsa definitions
// -------------------------------------------------------------------------

#[salsa::input]
struct SourceFile {
    #[returns(ref)]
    path: String,
    #[returns(ref)]
    text: String,
    #[returns(ref)]
    language: String,
}

#[salsa::tracked]
struct ParsedFile<'db> {
    #[returns(ref)]
    symbols: Vec<Symbol>,
    #[returns(ref)]
    refs: Vec<Ref>,
    #[returns(ref)]
    imports: Vec<Import>,
    line_count: usize,
    parsed_ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Symbol {
    qualified_name: String,
    short_name: String,
    kind: String,
    signature: Option<String>,
    visibility: Option<String>,
    start_line: usize,
    end_line: usize,
    docstring: Option<String>,
    cognitive: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Ref {
    name: String,
    line: usize,
    context_kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Import {
    raw_path: String,
    line: usize,
}

// Sorted vec of (lookup_name, file_path, qualified_name) — HashMap can't Hash
#[salsa::tracked]
struct SymbolIndex<'db> {
    #[returns(ref)]
    entries: Vec<(String, String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ResolvedRef {
    ref_name: String,
    ref_line: usize,
    target_file: String,
    target_symbol: String,
}

#[salsa::tracked]
struct ResolvedRefs<'db> {
    #[returns(ref)]
    refs: Vec<ResolvedRef>,
}

#[salsa::tracked]
struct FileOutline<'db> {
    #[returns(ref)]
    entries: Vec<OutlineEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct OutlineEntry {
    qualified_name: String,
    kind: String,
    signature: Option<String>,
    start_line: usize,
    end_line: usize,
}

#[salsa::tracked]
struct ImportEdges<'db> {
    #[returns(ref)]
    edges: Vec<(String, String)>,
}

#[salsa::input]
struct FileSet {
    #[returns(ref)]
    files: Vec<SourceFile>,
}

// -------------------------------------------------------------------------
// Tracked functions (the query graph)
// -------------------------------------------------------------------------

#[salsa::tracked]
fn parse_source_file(db: &dyn salsa::Database, file: SourceFile) -> ParsedFile<'_> {
    let text = file.text(db);
    let lang = file.language(db);
    let path = file.path(db);

    let result = parser::parse_file(text, lang, path)
        .unwrap_or_else(|_| parser::ParseResult {
            file_path: path.to_string(),
            language: lang.to_string(),
            symbols: vec![],
            references: vec![],
            imports: vec![],
            parsed_ok: false,
            line_count: text.lines().count(),
        });

    let symbols: Vec<Symbol> = result.symbols.into_iter().map(|s| Symbol {
        qualified_name: s.qualified_name,
        short_name: s.short_name,
        kind: s.kind.as_str().to_string(),
        signature: s.signature,
        visibility: s.visibility,
        start_line: s.start_line,
        end_line: s.end_line,
        docstring: s.docstring,
        cognitive: s.cognitive,
    }).collect();

    let refs: Vec<Ref> = result.references.into_iter().map(|r| Ref {
        name: r.name,
        line: r.line,
        context_kind: r.context_kind.as_str().to_string(),
    }).collect();

    let imports: Vec<Import> = result.imports.into_iter().map(|i| Import {
        raw_path: i.raw_path,
        line: i.line,
    }).collect();

    ParsedFile::new(db, symbols, refs, imports, result.line_count, result.parsed_ok)
}

#[salsa::tracked]
fn file_outline(db: &dyn salsa::Database, file: SourceFile) -> FileOutline<'_> {
    let parsed = parse_source_file(db, file);
    let entries: Vec<OutlineEntry> = parsed.symbols(db).iter().map(|s| OutlineEntry {
        qualified_name: s.qualified_name.clone(),
        kind: s.kind.clone(),
        signature: s.signature.clone(),
        start_line: s.start_line,
        end_line: s.end_line,
    }).collect();
    FileOutline::new(db, entries)
}

#[salsa::tracked]
fn build_symbol_index(db: &dyn salsa::Database, files: FileSet) -> SymbolIndex<'_> {
    let mut entries = Vec::new();
    for &file in files.files(db) {
        let parsed = parse_source_file(db, file);
        let path = file.path(db);
        for sym in parsed.symbols(db) {
            entries.push((sym.short_name.clone(), path.to_string(), sym.qualified_name.clone()));
            entries.push((sym.qualified_name.clone(), path.to_string(), sym.qualified_name.clone()));
        }
    }
    entries.sort();
    SymbolIndex::new(db, entries)
}

#[salsa::tracked]
fn resolve_file_refs<'db>(
    db: &'db dyn salsa::Database,
    file: SourceFile,
    index: SymbolIndex<'db>,
) -> ResolvedRefs<'db> {
    let parsed = parse_source_file(db, file);
    let entries = index.entries(db);
    let file_path = file.path(db);

    let resolved: Vec<ResolvedRef> = parsed.refs(db).iter().filter_map(|r| {
        // Binary search in sorted entries
        let start = entries.partition_point(|(name, _, _)| name.as_str() < r.name.as_str());
        let matches: Vec<_> = entries[start..].iter()
            .take_while(|(name, _, _)| name == &r.name)
            .collect();
        if matches.is_empty() { return None; }
        let target = matches.iter()
            .find(|(_, p, _)| p != file_path)
            .or(matches.first())
            .unwrap();
        Some(ResolvedRef {
            ref_name: r.name.clone(),
            ref_line: r.line,
            target_file: target.1.clone(),
            target_symbol: target.2.clone(),
        })
    }).collect();

    ResolvedRefs::new(db, resolved)
}

#[salsa::tracked]
fn file_import_edges(db: &dyn salsa::Database, file: SourceFile) -> ImportEdges<'_> {
    let parsed = parse_source_file(db, file);
    let path = file.path(db).to_string();
    let edges: Vec<(String, String)> = parsed.imports(db).iter()
        .map(|i| (path.clone(), i.raw_path.clone()))
        .collect();
    ImportEdges::new(db, edges)
}

// -------------------------------------------------------------------------
// File loading
// -------------------------------------------------------------------------

fn load_workspace_files(db: &dyn salsa::Database, root: &Path) -> Vec<SourceFile> {
    let ext_map: HashMap<&str, &str> = [("rs", "rust")].into_iter().collect();
    let mut files = Vec::new();
    walk_dir(root, root, &ext_map, &mut files, db);
    files
}

fn walk_dir(
    dir: &Path,
    root: &Path,
    ext_map: &HashMap<&str, &str>,
    files: &mut Vec<SourceFile>,
    db: &dyn salsa::Database,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with('.') || name == "target" || name == "build" {
                continue;
            }
            walk_dir(&path, root, ext_map, files, db);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if let Some(&lang) = ext_map.get(ext) {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let rel = path.strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string();
                    let sf = SourceFile::new(db, rel, text, lang.to_string());
                    files.push(sf);
                }
            }
        }
    }
}

// -------------------------------------------------------------------------
// Experiments
// -------------------------------------------------------------------------

fn experiment_single_file_incremental(db: &mut salsa::DatabaseImpl, files: &[SourceFile]) {
    println!("\n{SEP}");
    println!("Experiment: single-file incremental parse");
    println!("{SEP}");

    if files.is_empty() {
        println!("  No files to test");
        return;
    }

    let target = files.iter()
        .max_by_key(|f| f.text(db).len())
        .copied()
        .unwrap();
    let path = target.path(db).to_string();
    let original_len = target.text(db).len();
    println!("  Target: {path} ({original_len} bytes)");

    // Cold parse
    let t0 = Instant::now();
    let parsed = parse_source_file(db, target);
    let cold_time = t0.elapsed();
    let sym_count = parsed.symbols(db).len();
    let ref_count = parsed.refs(db).len();
    println!("  Cold parse: {:?} ({sym_count} symbols, {ref_count} refs)", cold_time);

    // Warm read (no change)
    let t0 = Instant::now();
    let parsed2 = parse_source_file(db, target);
    let warm_time = t0.elapsed();
    println!("  Warm read (memoized): {:?}", warm_time);
    assert_eq!(parsed.symbols(db).len(), parsed2.symbols(db).len());

    // Cold outline
    let t0 = Instant::now();
    let outline = file_outline(db, target);
    let outline_cold = t0.elapsed();
    println!("  Outline (cold, depends on parse): {:?} ({} entries)", outline_cold, outline.entries(db).len());

    // Warm outline
    let t0 = Instant::now();
    let _outline2 = file_outline(db, target);
    let outline_warm = t0.elapsed();
    println!("  Outline (warm, memoized): {:?}", outline_warm);

    // Mutate: add a comment at the end
    let original_text = target.text(db).clone();
    let new_text = format!("{original_text}\n// spike modification\n");
    target.set_text(db).to(new_text);

    let t0 = Instant::now();
    let parsed3 = parse_source_file(db, target);
    let reparse_time = t0.elapsed();
    let new_sym_count = parsed3.symbols(db).len();
    println!("  Reparse after trivial change: {:?} ({new_sym_count} symbols)", reparse_time);

    // Outline after mutation — does salsa skip recomputing if symbols unchanged?
    let t0 = Instant::now();
    let outline3 = file_outline(db, target);
    let outline_reparse = t0.elapsed();
    println!("  Outline after mutation: {:?} ({} entries)", outline_reparse, outline3.entries(db).len());

    // Restore
    target.set_text(db).to(original_text);

    println!("\n  Speedup (warm/cold): {:.1}x",
        cold_time.as_nanos() as f64 / warm_time.as_nanos().max(1) as f64);
    println!("  Key question: outline reparse should be fast if symbols");
    println!("  didn't change (salsa compares memoized ParsedFile output).");
}

fn experiment_bulk_parse(db: &mut salsa::DatabaseImpl, files: &[SourceFile]) {
    println!("\n{SEP}");
    println!("Experiment: bulk parse all files");
    println!("{SEP}");

    // Cold: parse all files
    let t0 = Instant::now();
    let mut total_syms = 0;
    let mut total_refs = 0;
    for &f in files {
        let parsed = parse_source_file(db, f);
        total_syms += parsed.symbols(db).len();
        total_refs += parsed.refs(db).len();
    }
    let cold_time = t0.elapsed();
    println!("  Cold parse all {} files: {:?}", files.len(), cold_time);
    println!("  Total: {total_syms} symbols, {total_refs} refs");

    // Warm: re-read all
    let t0 = Instant::now();
    for &f in files {
        let _ = parse_source_file(db, f);
    }
    let warm_time = t0.elapsed();
    println!("  Warm read all (memoized): {:?}", warm_time);
    println!("  Speedup: {:.1}x", cold_time.as_nanos() as f64 / warm_time.as_nanos().max(1) as f64);

    // Mutate one file, re-read all
    if let Some(&target) = files.first() {
        let original_text = target.text(db).clone();
        let new_text = format!("{original_text}\n// spike\n");
        target.set_text(db).to(new_text);

        let t0 = Instant::now();
        for &f in files {
            let _ = parse_source_file(db, f);
        }
        let one_changed_time = t0.elapsed();
        println!("  Re-read all after 1 file changed: {:?}", one_changed_time);
        println!("  (Salsa reparses only the changed file, returns memoized for rest)");

        target.set_text(db).to(original_text);
    }
}

fn experiment_cross_file_resolution(db: &mut salsa::DatabaseImpl, files: &[SourceFile]) {
    println!("\n{SEP}");
    println!("Experiment: cross-file reference resolution");
    println!("{SEP}");

    if files.len() < 2 {
        println!("  Need at least 2 files");
        return;
    }

    let file_set = FileSet::new(db, files.to_vec());

    // Build symbol index (cold)
    let t0 = Instant::now();
    let index = build_symbol_index(db, file_set);
    let index_cold = t0.elapsed();
    let index_size = index.entries(db).len();
    println!("  Symbol index (cold): {:?} ({index_size} name entries)", index_cold);

    // Warm
    let t0 = Instant::now();
    let _index2 = build_symbol_index(db, file_set);
    let index_warm = t0.elapsed();
    println!("  Symbol index (warm): {:?}", index_warm);

    // Resolve refs for all files (cold)
    let t0 = Instant::now();
    let mut total_resolved = 0;
    for &f in files {
        let resolved = resolve_file_refs(db, f, index);
        total_resolved += resolved.refs(db).len();
    }
    let resolve_cold = t0.elapsed();
    println!("  Resolve all refs (cold): {:?} ({total_resolved} resolved)", resolve_cold);

    // Warm
    let t0 = Instant::now();
    for &f in files {
        let _ = resolve_file_refs(db, f, index);
    }
    let resolve_warm = t0.elapsed();
    println!("  Resolve all refs (warm): {:?}", resolve_warm);

    // Mutate one file, see what Salsa re-does
    let target = files[0];
    let target_path = target.path(db).to_string();
    println!("\n  Mutation test: changing {target_path}");

    let original_text = target.text(db).clone();
    let new_text = format!("{original_text}\nfn salsa_spike_injected() {{}}\n");
    target.set_text(db).to(new_text);

    // Symbol index must recompute because one file changed
    let t0 = Instant::now();
    let index_after = build_symbol_index(db, file_set);
    let index_recompute = t0.elapsed();
    let new_size = index_after.entries(db).len();
    println!("  Symbol index after mutation: {:?} ({new_size} entries, was {index_size})", index_recompute);

    // Re-resolve all refs
    let t0 = Instant::now();
    let mut reresolved = 0;
    for &f in files {
        let resolved = resolve_file_refs(db, f, index_after);
        reresolved += resolved.refs(db).len();
    }
    let resolve_after = t0.elapsed();
    println!("  Resolve refs after mutation: {:?} ({reresolved} resolved)", resolve_after);

    println!("\n  Cross-file assessment:");
    println!("  The symbol index aggregates ALL files — any file change");
    println!("  invalidates it. This is the same bottleneck as current sutra.");
    println!("  Salsa helps within a file (parse → outline stays memoized)");
    println!("  but cross-file aggregation is still O(all files).");

    // Restore
    target.set_text(db).to(original_text);
}

fn experiment_import_edges(db: &mut salsa::DatabaseImpl, files: &[SourceFile]) {
    println!("\n{SEP}");
    println!("Experiment: import edge extraction (DD handoff point)");
    println!("{SEP}");

    let t0 = Instant::now();
    let mut total_edges = 0;
    for &f in files {
        let edges = file_import_edges(db, f);
        total_edges += edges.edges(db).len();
    }
    let cold_time = t0.elapsed();
    println!("  Import edges (cold): {:?} ({total_edges} edges from {} files)", cold_time, files.len());

    let t0 = Instant::now();
    for &f in files {
        let _ = file_import_edges(db, f);
    }
    let warm_time = t0.elapsed();
    println!("  Import edges (warm): {:?}", warm_time);

    println!("\n  DD handoff: Salsa produces per-file edge lists.");
    println!("  DD would consume the union as its input collection.");
    println!("  On file change, only that file's edges recompute in Salsa,");
    println!("  then DD incrementally updates its maintained views.");
}

fn experiment_memory_overhead(db: &salsa::DatabaseImpl, files: &[SourceFile]) {
    println!("\n{SEP}");
    println!("Experiment: memory overhead assessment");
    println!("{SEP}");

    let mut total_source_bytes: usize = 0;
    let mut total_syms = 0;
    let mut total_refs = 0;
    for &f in files {
        total_source_bytes += f.text(db).len();
        let parsed = parse_source_file(db, f);
        total_syms += parsed.symbols(db).len();
        total_refs += parsed.refs(db).len();
    }

    let est_sym_bytes = total_syms * 200;
    let est_ref_bytes = total_refs * 100;
    let est_total = total_source_bytes + est_sym_bytes + est_ref_bytes;

    println!("  Files: {}", files.len());
    println!("  Source text: {} KB", total_source_bytes / 1024);
    println!("  Symbols: {total_syms} (~{} KB est.)", est_sym_bytes / 1024);
    println!("  Refs: {total_refs} (~{} KB est.)", est_ref_bytes / 1024);
    println!("  Estimated Salsa resident: ~{} KB", est_total / 1024);
    println!("\n  SQLite comparison: on-disk, loaded on demand, no memory pressure.");
    println!("  Salsa: all in memory, instant access, scales with codebase.");
    let feasible = est_total < 100 * 1024 * 1024;
    println!("  For {} files: {}", files.len(),
        if feasible { "well within limits" } else { "approaching limits — consider eviction" });
}

fn print_comparison_matrix(files: &[SourceFile]) {
    println!("\n{SEP}");
    println!("Comparison matrix: Salsa vs DD vs SQLite");
    println!("{SEP}");

    println!("
  | Capability                  | SQLite (current) | Salsa            | DD               |
  |-----------------------------|------------------|------------------|------------------|
  | Per-file parse memoization  | hash-based skip  | auto dependency  | N/A              |
  | Cross-file invalidation     | manual           | automatic*       | automatic        |
  | On-demand queries           | SQL              | tracked fns      | probe (awkward)  |
  | Maintained graph views      | recompute        | recompute        | automatic        |
  | Memory model                | disk (on-demand) | all in memory    | all in memory    |
  | Persistence across restart  | yes              | no               | no               |
  | Incremental granularity     | file-level       | query-level      | tuple-level      |
  | Learning curve              | low              | medium           | high             |
  | Dependency weight           | 0 (bundled)      | ~19 crates       | ~15 crates       |

  *Cross-file: Salsa tracks deps automatically but aggregation queries
   (symbol index) that read all files still recompute when any file changes.");

    println!("\n  Sweet spots:");
    println!("    SQLite: persistence, large codebases, cross-restart state");
    println!("    Salsa:  per-file pipeline (parse→symbols→outline), session cache");
    println!("    DD:     graph analytics (cycles, transitive closure, rollups)");

    println!("\n  Proposed architecture (if complement):");
    println!("    Salsa: source text → parse → symbols → refs → outline");
    println!("    DD:    file graph → cycles → constraints → rollups");
    println!("    SQLite: persistence, snapshots, cross-restart bootstrap");
    println!("    Boundary: Salsa's per-file outputs seed DD + SQLite");
    println!("    {} files in this workspace.", files.len());
}

// -------------------------------------------------------------------------
// Main
// -------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let workspace_root = args.get(1).map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap());

    println!("=== Salsa Incremental Computation Spike ===");
    println!("Root: {}", workspace_root.display());

    let mut db = salsa::DatabaseImpl::new();

    let t0 = Instant::now();
    let files = load_workspace_files(&db, &workspace_root);
    println!("Loaded {} files in {:?}", files.len(), t0.elapsed());

    experiment_single_file_incremental(&mut db, &files);
    experiment_bulk_parse(&mut db, &files);
    experiment_cross_file_resolution(&mut db, &files);
    experiment_import_edges(&mut db, &files);
    experiment_memory_overhead(&db, &files);
    print_comparison_matrix(&files);

    println!("\n{SEP}");
    println!("All experiments complete.");
}
