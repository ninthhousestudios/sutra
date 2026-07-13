use std::collections::HashMap;

use sutra::db::ConstraintRatchetRow;
use sutra::db::{
    Db, InsertImportParams, InsertRefParams, InsertSymbolParams, SnapshotComponentRow,
    SnapshotFileRow, SnapshotParams, TABLE_REGISTRY, TablePartition,
};
use sutra::workspace::WorkspaceEntry;

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

#[test]
fn test_open_for_workspace_rejects_index_inside_workspace_root() {
    let dir = tempfile::tempdir().unwrap();
    let workspace_root = dir.path().join("project");
    std::fs::create_dir_all(&workspace_root).unwrap();

    let entry = WorkspaceEntry {
        id: "project".to_string(),
        root: workspace_root.clone(),
        languages: vec!["rust".to_string()],
    };

    let result = Db::open_for_workspace(&entry, dir.path());
    assert!(
        result.is_err(),
        "expected workspace-aware DB open to reject index inside workspace root"
    );
    assert!(
        !workspace_root.join("index.db").exists(),
        "validation must happen before SQLite creates index.db"
    );
}

fn seed_file(db: &Db, path: &str) -> i64 {
    db.upsert_file(path, "rust", "abc123", 100, true).unwrap()
}

fn seed_symbol(db: &Db, file_id: i64, qn: &str, sn: &str, kind: &str) -> i64 {
    db.insert_symbol(&InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind,
        signature: None,
        signature_hash: None,
        visibility: None,
        start_line: 1,
        start_col: 0,
        end_line: 10,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    })
    .unwrap()
}

// ---------------------------------------------------------------------------
// File operations
// ---------------------------------------------------------------------------

#[test]
fn test_upsert_file_insert_and_update() {
    let (_dir, db) = setup_db();

    let id1 = db
        .upsert_file("src/lib.rs", "rust", "hash1", 50, true)
        .unwrap();
    let row = db.file_by_path("src/lib.rs").unwrap().unwrap();
    assert_eq!(row.id, id1);
    assert_eq!(row.content_hash, "hash1");
    assert_eq!(row.line_count, 50);

    let id2 = db
        .upsert_file("src/lib.rs", "rust", "hash2", 60, true)
        .unwrap();
    assert_eq!(id1, id2);
    let row2 = db.file_by_path("src/lib.rs").unwrap().unwrap();
    assert_eq!(row2.content_hash, "hash2");
    assert_eq!(row2.line_count, 60);
}

#[test]
fn test_file_by_id_not_found() {
    let (_dir, db) = setup_db();
    assert!(db.file_by_id(999).unwrap().is_none());
}

#[test]
fn test_all_files() {
    let (_dir, db) = setup_db();
    seed_file(&db, "a.rs");
    seed_file(&db, "b.rs");
    seed_file(&db, "c.rs");
    assert_eq!(db.all_files().unwrap().len(), 3);
}

#[test]
fn test_update_rollups() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/main.rs");
    db.update_rollups(fid, 5, 12).unwrap();
    let row = db.file_by_id(fid).unwrap().unwrap();
    assert_eq!(row.fan_in_files, 5);
    assert_eq!(row.blast_radius, 12);
}

#[test]
fn test_delete_file_cascade() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    let sid = seed_symbol(&db, fid, "lib::foo", "foo", "function");
    db.insert_ref(fid, Some(sid), None, 5, 0, "call").unwrap();
    db.insert_import(fid, "std::io", None, 1, "use").unwrap();

    db.delete_file_cascade(fid).unwrap();

    assert!(db.file_by_id(fid).unwrap().is_none());
    assert!(db.symbol_by_id(sid).unwrap().is_none());
    assert!(db.find_refs_in_file(fid).unwrap().is_empty());
    assert!(db.imports_for_file(fid).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Symbol operations
// ---------------------------------------------------------------------------

#[test]
fn test_insert_and_lookup_symbol() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    let sid = db
        .insert_symbol(&InsertSymbolParams {
            file_id: fid,
            qualified_name: "lib::bar",
            short_name: "bar",
            kind: "function",
            signature: Some("fn bar()"),
            signature_hash: None,
            visibility: Some("pub"),
            start_line: 1,
            start_col: 0,
            end_line: 5,
            end_col: 0,
            parent_symbol_id: None,
            docstring: Some("docs"),
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        })
        .unwrap();

    let by_id = db.symbol_by_id(sid).unwrap().unwrap();
    assert_eq!(&*by_id.qualified_name, "lib::bar");
    assert_eq!(&*by_id.short_name, "bar");
    assert_eq!(&*by_id.kind, "function");
    assert_eq!(by_id.visibility.as_deref(), Some("pub"));
    assert_eq!(by_id.docstring.as_deref(), Some("docs"));

    let by_qn = db.symbol_by_qualified_name("lib::bar").unwrap().unwrap();
    assert_eq!(by_qn.id, sid);
}

#[test]
fn test_find_symbols_by_name_exact() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_symbol(&db, fid, "lib::alpha", "alpha", "function");
    seed_symbol(&db, fid, "lib::beta", "beta", "function");

    let results = db.find_symbols_by_name("alpha", None, 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(&*results[0].short_name, "alpha");
}

#[test]
fn test_find_symbols_by_name_with_kind_filter() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_symbol(&db, fid, "lib::process", "process", "function");
    seed_symbol(&db, fid, "mod::process", "process", "struct");

    let fns = db
        .find_symbols_by_name("process", Some("function"), 10)
        .unwrap();
    assert_eq!(fns.len(), 1);
    assert_eq!(&*fns[0].kind, "function");

    let structs = db
        .find_symbols_by_name("process", Some("struct"), 10)
        .unwrap();
    assert_eq!(structs.len(), 1);
    assert_eq!(&*structs[0].kind, "struct");
}

#[test]
fn test_find_symbols_by_name_fts_fallback() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_symbol(&db, fid, "lib::my_function", "my_function", "function");

    let results = db.find_symbols_by_name("my_func", None, 10).unwrap();
    assert!(!results.is_empty());
    assert_eq!(&*results[0].short_name, "my_function");
}

#[test]
fn test_find_symbols_by_file() {
    let (_dir, db) = setup_db();
    let fid1 = seed_file(&db, "src/a.rs");
    let fid2 = seed_file(&db, "src/b.rs");
    seed_symbol(&db, fid1, "a::one", "one", "function");
    seed_symbol(&db, fid1, "a::two", "two", "function");
    seed_symbol(&db, fid2, "b::three", "three", "function");

    let syms = db.find_symbols_by_file(fid1).unwrap();
    assert_eq!(syms.len(), 2);
    assert!(syms.iter().all(|s| s.file_id == fid1));
}

// ---------------------------------------------------------------------------
// New batch/helper methods
// ---------------------------------------------------------------------------

#[test]
fn test_symbol_counts_by_file() {
    let (_dir, db) = setup_db();
    let fa = seed_file(&db, "a.rs");
    let fb = seed_file(&db, "b.rs");
    let fc = seed_file(&db, "c.rs");

    seed_symbol(&db, fa, "a::one", "one", "function");
    seed_symbol(&db, fa, "a::two", "two", "function");
    seed_symbol(&db, fb, "b::one", "one", "function");
    seed_symbol(&db, fb, "b::two", "two", "function");
    seed_symbol(&db, fb, "b::three", "three", "function");
    seed_symbol(&db, fc, "c::one", "one", "function");

    let counts: HashMap<i64, i64> = db.symbol_counts_by_file().unwrap();
    assert_eq!(counts[&fa], 2);
    assert_eq!(counts[&fb], 3);
    assert_eq!(counts[&fc], 1);
}

#[test]
fn test_all_symbols_summary() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    let id1 = seed_symbol(&db, fid, "lib::alpha", "alpha", "function");
    let id2 = seed_symbol(&db, fid, "lib::beta", "beta", "struct");
    let id3 = seed_symbol(&db, fid, "lib::gamma", "gamma", "function");

    let summary = db.all_symbols_summary().unwrap();
    assert_eq!(summary.len(), 3);

    let ids: Vec<i64> = summary.iter().map(|s| s.id).collect();
    assert!(ids.contains(&id1));
    assert!(ids.contains(&id2));
    assert!(ids.contains(&id3));

    let qnames: Vec<&str> = summary.iter().map(|s| s.qualified_name.as_str()).collect();
    assert!(qnames.contains(&"lib::alpha"));

    let snames: Vec<&str> = summary.iter().map(|s| s.short_name.as_str()).collect();
    assert!(snames.contains(&"beta"));

    let kinds: Vec<&str> = summary.iter().map(|s| s.kind.as_str()).collect();
    assert!(kinds.contains(&"function"));
    assert!(kinds.contains(&"struct"));
}

#[test]
fn test_resolve_symbol_by_qualified_name() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    let sid = seed_symbol(&db, fid, "lib::exact_match", "exact_match", "function");

    let result = db
        .resolve_symbol("lib::exact_match", None)
        .unwrap()
        .unwrap();
    assert_eq!(result.id, sid);
}

#[test]
fn test_resolve_symbol_by_short_name() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    let sid = seed_symbol(&db, fid, "lib::fallback_fn", "fallback_fn", "function");

    let result = db.resolve_symbol("fallback_fn", None).unwrap().unwrap();
    assert_eq!(result.id, sid);
}

#[test]
fn test_resolve_symbol_not_found() {
    let (_dir, db) = setup_db();
    assert!(
        db.resolve_symbol("totally_nonexistent_zzzz", None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_find_enclosing_symbol_exact() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    let sid = db
        .insert_symbol(&InsertSymbolParams {
            file_id: fid,
            qualified_name: "lib::outer",
            short_name: "outer",
            kind: "function",
            signature: None,
            signature_hash: None,
            visibility: None,
            start_line: 10,
            start_col: 0,
            end_line: 20,
            end_col: 0,
            parent_symbol_id: None,
            docstring: None,
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        })
        .unwrap();

    let result = db.find_enclosing_symbol(fid, 15).unwrap().unwrap();
    assert_eq!(result.id, sid);
}

#[test]
fn test_find_enclosing_symbol_nested() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    let outer_id = db
        .insert_symbol(&InsertSymbolParams {
            file_id: fid,
            qualified_name: "lib::outer",
            short_name: "outer",
            kind: "function",
            signature: None,
            signature_hash: None,
            visibility: None,
            start_line: 1,
            start_col: 0,
            end_line: 50,
            end_col: 0,
            parent_symbol_id: None,
            docstring: None,
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        })
        .unwrap();
    let inner_id = db
        .insert_symbol(&InsertSymbolParams {
            file_id: fid,
            qualified_name: "lib::inner",
            short_name: "inner",
            kind: "function",
            signature: None,
            signature_hash: None,
            visibility: None,
            start_line: 10,
            start_col: 0,
            end_line: 20,
            end_col: 0,
            parent_symbol_id: Some(outer_id),
            docstring: None,
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        })
        .unwrap();

    let result = db.find_enclosing_symbol(fid, 15).unwrap().unwrap();
    assert_eq!(
        result.id, inner_id,
        "should find narrowest enclosing symbol"
    );
}

#[test]
fn test_find_enclosing_symbol_outside() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    db.insert_symbol(&InsertSymbolParams {
        file_id: fid,
        qualified_name: "lib::fn1",
        short_name: "fn1",
        kind: "function",
        signature: None,
        signature_hash: None,
        visibility: None,
        start_line: 1,
        start_col: 0,
        end_line: 20,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    })
    .unwrap();

    assert!(db.find_enclosing_symbol(fid, 100).unwrap().is_none());
}

// ---------------------------------------------------------------------------
// Ref operations
// ---------------------------------------------------------------------------

#[test]
fn test_insert_and_find_refs() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    let sid = seed_symbol(&db, fid, "lib::foo", "foo", "function");

    db.insert_ref(fid, Some(sid), None, 5, 0, "call").unwrap();
    db.insert_ref(fid, Some(sid), None, 8, 2, "call").unwrap();

    let refs = db.find_refs_to_symbol(sid).unwrap();
    assert_eq!(refs.len(), 2);
    assert!(refs.iter().all(|r| r.target_symbol_id == Some(sid)));
}

#[test]
fn test_find_refs_in_file() {
    let (_dir, db) = setup_db();
    let fa = seed_file(&db, "a.rs");
    let fb = seed_file(&db, "b.rs");
    let sid = seed_symbol(&db, fa, "a::foo", "foo", "function");

    db.insert_ref(fa, Some(sid), None, 3, 0, "call").unwrap();
    db.insert_ref(fb, Some(sid), None, 7, 0, "call").unwrap();

    let refs_a = db.find_refs_in_file(fa).unwrap();
    assert_eq!(refs_a.len(), 1);
    assert_eq!(refs_a[0].file_id, fa);
}

#[test]
fn test_delete_refs_by_file() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    let sid = seed_symbol(&db, fid, "lib::foo", "foo", "function");

    db.insert_ref(fid, Some(sid), None, 1, 0, "call").unwrap();
    db.insert_ref(fid, Some(sid), None, 2, 0, "call").unwrap();

    db.delete_refs_by_file(fid).unwrap();
    assert!(db.find_refs_in_file(fid).unwrap().is_empty());
}

#[test]
fn test_find_files_referencing_symbols() {
    let (_dir, db) = setup_db();
    let fa = seed_file(&db, "a.rs");
    let fb = seed_file(&db, "b.rs");
    let fc = seed_file(&db, "c.rs");
    let sid = seed_symbol(&db, fc, "c::foo", "foo", "function");

    db.insert_ref(fa, Some(sid), None, 1, 0, "call").unwrap();
    db.insert_ref(fb, Some(sid), None, 2, 0, "call").unwrap();

    let mut referencing = db.find_files_referencing_symbols(&[sid]).unwrap();
    referencing.sort();
    let mut expected = vec![fa, fb];
    expected.sort();
    assert_eq!(referencing, expected);
}

// ---------------------------------------------------------------------------
// Import operations
// ---------------------------------------------------------------------------

#[test]
fn test_insert_and_query_imports() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");

    db.insert_import(fid, "std::io", None, 1, "use").unwrap();
    db.insert_import(fid, "std::fmt", None, 2, "use").unwrap();

    let imports = db.imports_for_file(fid).unwrap();
    assert_eq!(imports.len(), 2);
    let paths: Vec<&str> = imports.iter().map(|i| i.imported_path.as_str()).collect();
    assert!(paths.contains(&"std::io"));
    assert!(paths.contains(&"std::fmt"));
}

#[test]
fn test_import_edges() {
    let (_dir, db) = setup_db();
    let fa = seed_file(&db, "a.rs");
    let fb = seed_file(&db, "b.rs");
    let fc = seed_file(&db, "c.rs");

    db.insert_import(fa, "b", Some(fb), 1, "use").unwrap();
    db.insert_import(fa, "c", Some(fc), 2, "use").unwrap();
    db.insert_import(fb, "unresolved", None, 1, "use").unwrap();

    let mut edges = db.import_edges().unwrap();
    edges.sort();
    let mut expected = vec![(fa, fb), (fa, fc)];
    expected.sort();
    assert_eq!(edges, expected);
}

// ---------------------------------------------------------------------------
// Snapshot operations
// ---------------------------------------------------------------------------

#[test]
fn test_insert_snapshot_and_last_parse_time() {
    let (_dir, db) = setup_db();
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 10,
        symbols_extracted: 50,
        refs_extracted: 20,
        parse_errors: 0,
        duration_ms: 300,
        total_complexity: 0,
        dead_symbol_count: 0,
        hotspot_count: 0,
        health_score: 0.0,
        ..Default::default()
    })
    .unwrap();
    let ts = db.last_parse_time().unwrap();
    assert!(ts.is_some());
}

#[test]
fn test_snapshot_with_aggregates() {
    let (_dir, db) = setup_db();
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 10,
        symbols_extracted: 50,
        refs_extracted: 20,
        parse_errors: 0,
        duration_ms: 300,
        total_complexity: 42,
        dead_symbol_count: 5,
        hotspot_count: 3,
        health_score: 78.0,
        ..Default::default()
    })
    .unwrap();

    let snaps = db.latest_snapshots(1).unwrap();
    assert_eq!(snaps.len(), 1);
    let s = &snaps[0];
    assert_eq!(s.files_parsed, 10);
    assert_eq!(s.symbols_extracted, 50);
    assert_eq!(s.total_complexity, 42);
    assert_eq!(s.dead_symbol_count, 5);
    assert_eq!(s.hotspot_count, 3);
    assert!((s.health_score - 78.0).abs() < 0.01);
}

#[test]
fn test_latest_snapshots_ordering() {
    let (_dir, db) = setup_db();
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 10,
        symbols_extracted: 50,
        refs_extracted: 20,
        parse_errors: 0,
        duration_ms: 100,
        total_complexity: 10,
        dead_symbol_count: 1,
        hotspot_count: 0,
        health_score: 90.0,
        ..Default::default()
    })
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 20,
        symbols_extracted: 80,
        refs_extracted: 40,
        parse_errors: 1,
        duration_ms: 200,
        total_complexity: 20,
        dead_symbol_count: 3,
        hotspot_count: 2,
        health_score: 75.0,
        ..Default::default()
    })
    .unwrap();

    let snaps = db.latest_snapshots(2).unwrap();
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0].files_parsed, 20);
    assert_eq!(snaps[1].files_parsed, 10);
}

#[test]
fn test_snapshots_between() {
    let (_dir, db) = setup_db();
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 10,
        symbols_extracted: 50,
        refs_extracted: 20,
        parse_errors: 0,
        duration_ms: 100,
        total_complexity: 10,
        dead_symbol_count: 1,
        hotspot_count: 0,
        health_score: 90.0,
        ..Default::default()
    })
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 20,
        symbols_extracted: 80,
        refs_extracted: 40,
        parse_errors: 1,
        duration_ms: 200,
        total_complexity: 20,
        dead_symbol_count: 3,
        hotspot_count: 2,
        health_score: 75.0,
        ..Default::default()
    })
    .unwrap();

    let snaps = db.snapshots_between("2000-01-01", "2099-01-01").unwrap();
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[0].files_parsed, 10);
    assert_eq!(snaps[1].files_parsed, 20);
}

#[test]
fn test_trend_default_from_to() {
    let (_dir, db) = setup_db();
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 10,
        symbols_extracted: 50,
        refs_extracted: 20,
        parse_errors: 0,
        duration_ms: 100,
        total_complexity: 10,
        dead_symbol_count: 1,
        hotspot_count: 0,
        health_score: 90.0,
        ..Default::default()
    })
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 20,
        symbols_extracted: 80,
        refs_extracted: 40,
        parse_errors: 1,
        duration_ms: 200,
        total_complexity: 25,
        dead_symbol_count: 4,
        hotspot_count: 2,
        health_score: 75.0,
        ..Default::default()
    })
    .unwrap();

    let result = sutra::tools::trend::handle(
        &db,
        &sutra::tools::trend::TrendArgs {
            workspace: String::new(),
            from: None,
            to: None,
            path: None,
            limit: None,
        },
    )
    .unwrap();
    let deltas = &result["deltas"];
    assert_eq!(deltas["files_parsed"], 10);
    assert_eq!(deltas["symbols_extracted"], 30);
    assert_eq!(deltas["total_complexity"], 15);
    assert_eq!(deltas["dead_symbol_count"], 3);
    assert_eq!(deltas["hotspot_count"], 2);
    assert!((deltas["health_score"].as_f64().unwrap() - (-15.0)).abs() < 0.01);
}

#[test]
fn test_trend_insufficient_snapshots() {
    let (_dir, db) = setup_db();
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 10,
        symbols_extracted: 50,
        refs_extracted: 20,
        parse_errors: 0,
        duration_ms: 100,
        total_complexity: 10,
        dead_symbol_count: 1,
        hotspot_count: 0,
        health_score: 90.0,
        ..Default::default()
    })
    .unwrap();
    let result = sutra::tools::trend::handle(
        &db,
        &sutra::tools::trend::TrendArgs {
            workspace: String::new(),
            from: None,
            to: None,
            path: None,
            limit: None,
        },
    );
    assert!(result.is_err());
}

#[test]
fn test_pre_existing_snapshots_have_zero_aggregates() {
    let (_dir, db) = setup_db();
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 10,
        symbols_extracted: 50,
        refs_extracted: 20,
        parse_errors: 0,
        duration_ms: 300,
        total_complexity: 0,
        dead_symbol_count: 0,
        hotspot_count: 0,
        health_score: 0.0,
        ..Default::default()
    })
    .unwrap();
    let snaps = db.latest_snapshots(1).unwrap();
    assert_eq!(snaps[0].total_complexity, 0);
    assert_eq!(snaps[0].dead_symbol_count, 0);
    assert_eq!(snaps[0].hotspot_count, 0);
    assert!((snaps[0].health_score - 0.0).abs() < 0.01);
}

#[test]
fn test_last_parse_time_empty() {
    let (_dir, db) = setup_db();
    assert!(db.last_parse_time().unwrap().is_none());
}

#[test]
fn test_find_symbols_by_name_with_colons() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_symbol(&db, fid, "foo::bar", "bar", "function");

    let results = db.find_symbols_by_name("foo::bar", None, 10).unwrap();
    assert!(results.is_empty() || &*results[0].qualified_name == "foo::bar");

    let results = db
        .find_symbols_by_name("nonexistent::thing", None, 10)
        .unwrap();
    assert!(results.is_empty());
}

// ---------------------------------------------------------------------------
// Migration runner tests
// ---------------------------------------------------------------------------

#[test]
fn test_fresh_db_creates_schema_migrations() {
    let (_dir, db) = setup_db();
    let conn = db.conn_for_test();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    let expected = Db::migration_count() as i64;
    assert_eq!(
        count, expected,
        "fresh DB should register all {expected} existing migrations"
    );
}

#[test]
fn test_migration_reopen_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let _db1 = Db::open_unchecked("test", dir.path()).unwrap();
    drop(_db1);
    let db2 = Db::open_unchecked("test", dir.path()).unwrap();
    let conn = db2.conn_for_test();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    let expected = Db::migration_count() as i64;
    assert_eq!(
        count, expected,
        "reopen should not duplicate migration rows"
    );
}

#[test]
fn test_migration_hash_mismatch_errors() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_unchecked("test", dir.path()).unwrap();
        let conn = db.conn_for_test();
        conn.execute(
            "UPDATE schema_migrations SET content_hash = 'tampered' WHERE name = '0001_initial'",
            [],
        )
        .unwrap();
    }
    let result = Db::open_unchecked("test", dir.path());
    let msg = match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected hash mismatch error, got Ok"),
    };
    assert!(
        msg.contains("content hash mismatch"),
        "expected hash mismatch error, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Convention operations
// ---------------------------------------------------------------------------

#[test]
fn convention_upsert_and_retrieve() {
    let (_dir, db) = setup_db();
    db.upsert_convention("abc123", "kind:function", "has_sig", 42, 0.95, None)
        .unwrap();
    let rows = db.all_conventions().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "abc123");
    assert_eq!(rows[0].antecedent, "kind:function");
    assert_eq!(rows[0].consequent, "has_sig");
    assert_eq!(rows[0].support, 42);
    assert!((rows[0].confidence - 0.95).abs() < 1e-9);
}

#[test]
fn convention_upsert_updates_existing() {
    let (_dir, db) = setup_db();
    db.upsert_convention("abc123", "kind:function", "has_sig", 42, 0.95, None)
        .unwrap();
    db.upsert_convention("abc123", "kind:function", "has_sig", 50, 0.97, None)
        .unwrap();
    let rows = db.all_conventions().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].support, 50);
    assert!((rows[0].confidence - 0.97).abs() < 1e-9);
}

#[test]
fn convention_delete_stale() {
    let (_dir, db) = setup_db();
    db.upsert_convention("aaa", "a", "b", 10, 0.9, None)
        .unwrap();
    db.upsert_convention("bbb", "c", "d", 20, 0.95, None)
        .unwrap();
    db.upsert_convention("ccc", "e", "f", 30, 0.99, None)
        .unwrap();
    let deleted = db.delete_stale_conventions(&["aaa", "ccc"]).unwrap();
    assert_eq!(deleted, 1);
    let rows = db.all_conventions().unwrap();
    assert_eq!(rows.len(), 2);
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"aaa"));
    assert!(ids.contains(&"ccc"));
}

#[test]
fn test_table_registry_covers_all_tables() {
    let names: Vec<&str> = TABLE_REGISTRY.iter().map(|t| t.name).collect();
    assert!(names.contains(&"files"));
    assert!(names.contains(&"symbols"));
    assert!(names.contains(&"symbols_fts"));
    assert!(names.contains(&"refs"));
    assert!(names.contains(&"imports"));
    assert!(names.contains(&"snapshots"));
    assert!(names.contains(&"conventions"));

    for meta in TABLE_REGISTRY {
        assert!(
            meta.partition == TablePartition::Ephemeral
                || meta.partition == TablePartition::Durable,
        );
    }
}

#[test]
fn test_reindex_drops_ephemeral_tables() {
    let (_dir, db) = setup_db();

    let file_id = seed_file(&db, "src/main.rs");
    seed_symbol(&db, file_id, "main", "main", "function");
    db.upsert_convention("conv1", "kind:function", "has_sig", 10, 0.9, None)
        .unwrap();

    let files = db.all_files().unwrap();
    assert!(!files.is_empty());
    let conventions = db.all_conventions().unwrap();
    assert!(!conventions.is_empty());

    let dropped = db.reindex().unwrap();
    assert!(dropped.contains(&"files"));
    assert!(dropped.contains(&"symbols"));
    assert!(dropped.contains(&"symbols_fts"));
    assert!(dropped.contains(&"conventions"));

    let files = db.all_files().unwrap();
    assert!(
        files.is_empty(),
        "ephemeral files table should be empty after reindex"
    );
    let conventions = db.all_conventions().unwrap();
    assert!(
        conventions.is_empty(),
        "ephemeral conventions table should be empty after reindex"
    );

    let file_id = seed_file(&db, "src/lib.rs");
    assert!(file_id > 0, "should be able to insert after reindex");
    seed_symbol(&db, file_id, "lib_fn", "lib_fn", "function");
    db.upsert_convention("conv2", "kind:struct", "has_doc", 5, 0.8, None)
        .unwrap();
}

// --- Constraint waiver tests ---

#[test]
fn constraint_waiver_crud() {
    let (_dir, db) = setup_db();

    let id = db
        .create_constraint_waiver(
            "abc12345",
            Some("no-tool-daemon"),
            "src/tools/review.rs",
            None,
            "legacy code",
            "josh",
        )
        .unwrap();
    assert!(id > 0);

    let waivers = db.get_constraint_waivers(None).unwrap();
    assert_eq!(waivers.len(), 1);
    assert_eq!(&*waivers[0].constraint_id, "abc12345");
    assert_eq!(
        waivers[0].constraint_name.as_deref(),
        Some("no-tool-daemon")
    );
    assert_eq!(waivers[0].file_path, "src/tools/review.rs");
    assert!(waivers[0].symbol_qualified_name.is_none());
    assert_eq!(waivers[0].rationale, "legacy code");
    assert_eq!(waivers[0].waived_by, "josh");

    let filtered = db.get_constraint_waivers(Some("abc12345")).unwrap();
    assert_eq!(filtered.len(), 1);
    let empty = db.get_constraint_waivers(Some("nonexistent")).unwrap();
    assert!(empty.is_empty());

    let updated = db
        .update_constraint_waiver(id, "approved exception")
        .unwrap();
    assert!(updated);
    let waivers = db.get_constraint_waivers(None).unwrap();
    assert_eq!(waivers[0].rationale, "approved exception");

    let deleted = db.delete_constraint_waiver(id).unwrap();
    assert!(deleted);
    assert!(db.get_constraint_waivers(None).unwrap().is_empty());

    let not_found = db.delete_constraint_waiver(id).unwrap();
    assert!(!not_found);
}

#[test]
fn constraint_waiver_upsert_on_conflict() {
    let (_dir, db) = setup_db();

    let first_id = db
        .create_constraint_waiver("abc", None, "src/a.rs", None, "first reason", "alice")
        .unwrap();
    let second_id = db
        .create_constraint_waiver(
            "abc",
            Some("named"),
            "src/a.rs",
            None,
            "updated reason",
            "bob",
        )
        .unwrap();

    assert_eq!(
        first_id, second_id,
        "upsert must return the existing row's id"
    );

    let waivers = db.get_constraint_waivers(None).unwrap();
    assert_eq!(waivers.len(), 1);
    assert_eq!(waivers[0].id, first_id);
    assert_eq!(waivers[0].rationale, "updated reason");
    assert_eq!(waivers[0].waived_by, "bob");
    assert_eq!(waivers[0].constraint_name.as_deref(), Some("named"));
}

#[test]
fn constraint_waiver_file_scoping() {
    let (_dir, db) = setup_db();

    db.create_constraint_waiver("abc", None, "src/a.rs", None, "waiver a", "josh")
        .unwrap();
    db.create_constraint_waiver("abc", None, "src/b.rs", None, "waiver b", "josh")
        .unwrap();
    db.create_constraint_waiver("def", None, "src/a.rs", None, "waiver def", "josh")
        .unwrap();

    let for_a = db.get_constraint_waivers_for_file("src/a.rs").unwrap();
    assert_eq!(for_a.len(), 2);

    let for_b = db.get_constraint_waivers_for_file("src/b.rs").unwrap();
    assert_eq!(for_b.len(), 1);
    assert_eq!(&*for_b[0].constraint_id, "abc");

    let for_c = db.get_constraint_waivers_for_file("src/c.rs").unwrap();
    assert!(for_c.is_empty());
}

#[test]
fn constraint_waiver_symbol_scoping() {
    let (_dir, db) = setup_db();

    db.create_constraint_waiver("abc", None, "src/a.rs", None, "file-level", "josh")
        .unwrap();
    db.create_constraint_waiver(
        "abc",
        None,
        "src/a.rs",
        Some("Foo::bar"),
        "symbol-level",
        "josh",
    )
    .unwrap();

    let waivers = db.get_constraint_waivers(Some("abc")).unwrap();
    assert_eq!(waivers.len(), 2);

    // Two NULL-symbol waivers for different constraints are distinct
    db.create_constraint_waiver(
        "def",
        None,
        "src/a.rs",
        None,
        "different constraint",
        "josh",
    )
    .unwrap();
    let all = db.get_constraint_waivers(None).unwrap();
    assert_eq!(all.len(), 3);
}

#[test]
fn constraint_waiver_survives_reindex() {
    let (_dir, db) = setup_db();

    db.create_constraint_waiver("abc", Some("no-cycles"), "src/a.rs", None, "legacy", "josh")
        .unwrap();

    db.reindex().unwrap();

    let waivers = db.get_constraint_waivers(None).unwrap();
    assert_eq!(waivers.len(), 1, "constraint waiver should survive reindex");
    assert_eq!(&*waivers[0].constraint_id, "abc");
    assert_eq!(waivers[0].rationale, "legacy");
}

#[test]
fn constraint_waiver_orphan_detection() {
    let (_dir, db) = setup_db();

    db.create_constraint_waiver("abc", None, "src/a.rs", None, "active waiver", "josh")
        .unwrap();
    db.create_constraint_waiver("def", None, "src/b.rs", None, "orphan waiver", "josh")
        .unwrap();
    db.create_constraint_waiver("ghi", None, "src/c.rs", None, "another orphan", "josh")
        .unwrap();

    let orphans = db.reconcile_orphaned_constraint_waivers(&["abc"]).unwrap();
    assert_eq!(orphans.len(), 2);
    let orphan_ids: Vec<&str> = orphans.iter().map(|w| &*w.constraint_id).collect();
    assert!(orphan_ids.contains(&"def"));
    assert!(orphan_ids.contains(&"ghi"));

    // All active → no orphans
    let orphans = db
        .reconcile_orphaned_constraint_waivers(&["abc", "def", "ghi"])
        .unwrap();
    assert!(orphans.is_empty());

    // No active → all orphans
    let orphans = db.reconcile_orphaned_constraint_waivers(&[]).unwrap();
    assert_eq!(orphans.len(), 3);
}

// ---------------------------------------------------------------------------
// Symbol uniqueness and heal
// ---------------------------------------------------------------------------

#[test]
fn test_insert_symbol_upserts_on_conflict() {
    let (_dir, db) = setup_db();
    let file_id = seed_file(&db, "test.rs");
    let id1 = db
        .insert_symbol(&InsertSymbolParams {
            file_id,
            qualified_name: "foo",
            short_name: "foo",
            kind: "function",
            signature: Some("fn foo()"),
            signature_hash: None,
            visibility: Some("pub"),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: None,
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        })
        .unwrap();

    let id2 = db
        .insert_symbol(&InsertSymbolParams {
            file_id,
            qualified_name: "foo",
            short_name: "foo",
            kind: "function",
            signature: Some("fn foo() -> i32"),
            signature_hash: None,
            visibility: Some("pub"),
            start_line: 1,
            start_col: 0,
            end_line: 15,
            end_col: 0,
            parent_symbol_id: None,
            docstring: None,
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        })
        .unwrap();

    assert_eq!(id1, id2, "ON CONFLICT should return same id");
    let sym = db.symbol_by_id(id1).unwrap().unwrap();
    assert_eq!(sym.end_line, 15, "conflicting insert should update columns");
    assert_eq!(
        sym.signature.as_deref(),
        Some("fn foo() -> i32"),
        "signature should be updated"
    );
}

#[test]
fn test_replace_file_data_atomic() {
    let (_dir, db) = setup_db();
    let symbols = vec![InsertSymbolParams {
        file_id: 0,
        qualified_name: "bar",
        short_name: "bar",
        kind: "function",
        signature: None,
        signature_hash: None,
        visibility: None,
        start_line: 1,
        start_col: 0,
        end_line: 5,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    }];
    let parents = vec![None];
    let imports = vec![InsertImportParams {
        imported_path: "std::io",
        line: 1,
        kind: "use",
    }];
    let refs = vec![InsertRefParams {
        unresolved_name: Some("println"),
        line: 3,
        col: 4,
        context_kind: "call",
    }];

    let (file_id, sym_count) = db
        .replace_file_data(
            "test.rs", "rust", "hash1", 5, true, &symbols, &parents, &imports, &refs,
        )
        .unwrap();
    assert!(file_id > 0);
    assert_eq!(sym_count, 1);

    // Replace again with different data — should not duplicate.
    let (file_id2, sym_count2) = db
        .replace_file_data(
            "test.rs", "rust", "hash2", 10, true, &symbols, &parents, &imports, &refs,
        )
        .unwrap();
    assert!(file_id2 > 0);
    assert_eq!(sym_count2, 1);

    let all_syms = db.find_symbols_by_file(file_id2).unwrap();
    assert_eq!(all_syms.len(), 1, "no duplicate symbols after re-replace");
}

#[test]
fn test_heal_duplicate_symbols_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open_unchecked("test", dir.path()).unwrap();
        let file_id = db.upsert_file("dup.rs", "rust", "abc", 10, true).unwrap();
        // Bypass the UNIQUE index by inserting with different start_lines first,
        // then manually updating to create duplicates.
        let conn = db.conn_for_test();
        conn.execute(
            "INSERT INTO symbols (file_id, qualified_name, short_name, kind, start_line, start_col, end_line, end_col, flags)
             VALUES (?1, 'dup_sym', 'dup_sym', 'function', 1, 0, 10, 0, 0)",
            rusqlite::params![file_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols_fts (symbol_id, short_name, qualified_name, docstring)
             VALUES (last_insert_rowid(), 'dup_sym', 'dup_sym', NULL)",
            [],
        )
        .unwrap();
        // Drop the unique index so we can insert a real duplicate.
        conn.execute("DROP INDEX IF EXISTS idx_symbols_unique_file_name_line", [])
            .unwrap();
        conn.execute(
            "INSERT INTO symbols (file_id, qualified_name, short_name, kind, start_line, start_col, end_line, end_col, flags)
             VALUES (?1, 'dup_sym', 'dup_sym', 'function', 1, 0, 10, 0, 0)",
            rusqlite::params![file_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO symbols_fts (symbol_id, short_name, qualified_name, docstring)
             VALUES (last_insert_rowid(), 'dup_sym', 'dup_sym', NULL)",
            [],
        )
        .unwrap();
        drop(conn);

        let syms = db.find_symbols_by_file(file_id).unwrap();
        assert_eq!(syms.len(), 2, "should have 2 duplicate symbols before heal");
    }

    // Reopen — heal_duplicate_symbols runs at startup.
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    let file_id = db.file_by_path("dup.rs").unwrap().unwrap().id;
    let syms = db.find_symbols_by_file(file_id).unwrap();
    assert_eq!(syms.len(), 1, "heal should remove duplicate, leaving one");
}

#[test]
fn test_snapshot_pruning() {
    let (_dir, db) = setup_db();
    let keep = 3usize;
    let total = keep + 5;

    let mut ids = Vec::new();
    for i in 0..total {
        let id = db
            .insert_snapshot_atomic(
                &SnapshotParams {
                    files_parsed: i as i64,
                    symbols_extracted: 0,
                    refs_extracted: 0,
                    parse_errors: 0,
                    duration_ms: 0,
                    total_complexity: 0,
                    dead_symbol_count: 0,
                    hotspot_count: 0,
                    health_score: 0.0,
                    ..Default::default()
                },
                &[SnapshotFileRow {
                    file_id: 1,
                    file_path: "a.rs".into(),
                    score: i as f64,
                    category_scores: "{}".into(),
                }],
                &[SnapshotComponentRow {
                    component_id: "comp".into(),
                    component_name: "comp".into(),
                    score: i as f64,
                    member_count: 1,
                    total_nloc: 10,
                }],
            )
            .unwrap();
        ids.push(id);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let before = db.latest_snapshots(100).unwrap();
    assert_eq!(before.len(), total, "all snapshots present before pruning");

    let pruned = db.prune_snapshots(keep).unwrap();
    assert_eq!(pruned, total - keep, "should prune {}", total - keep);

    let snaps = db.latest_snapshots(100).unwrap();
    assert_eq!(snaps.len(), keep, "should retain exactly {keep} snapshots");

    let kept_ids: Vec<i64> = snaps.iter().map(|s| s.id).collect();
    for &id in &ids[..total - keep] {
        assert!(
            !kept_ids.contains(&id),
            "old snapshot {id} should be pruned"
        );
    }
    for &id in &ids[total - keep..] {
        assert!(
            kept_ids.contains(&id),
            "recent snapshot {id} should survive"
        );
    }

    let pruned_id = ids[0];
    let surviving_id = *ids.last().unwrap();
    assert!(
        db.snapshot_file_scores(pruned_id).unwrap().is_empty(),
        "child file rows for pruned snapshot should be gone"
    );
    assert!(
        db.snapshot_component_scores(pruned_id).unwrap().is_empty(),
        "child component rows for pruned snapshot should be gone"
    );
    assert_eq!(
        db.snapshot_file_scores(surviving_id).unwrap().len(),
        1,
        "child file rows for surviving snapshot should remain"
    );
    assert_eq!(
        db.snapshot_component_scores(surviving_id).unwrap().len(),
        1,
        "child component rows for surviving snapshot should remain"
    );
}

// ---------------------------------------------------------------------------
// Constraint ratchet registry
// ---------------------------------------------------------------------------

#[test]
fn ratchet_upsert_and_get() {
    let (_dir, db) = setup_db();
    db.upsert_constraint_ratchet(
        "abc12345",
        Some("no-clone"),
        "forbidden_dep: a → b",
        "blocking",
    )
    .unwrap();
    let row = db.get_constraint_ratchet("abc12345").unwrap().unwrap();
    assert_eq!(&*row.constraint_id, "abc12345");
    assert_eq!(row.name.as_deref(), Some("no-clone"));
    assert_eq!(row.rendered_description, "forbidden_dep: a → b");
    assert_eq!(row.severity_floor, "blocking");
    assert!(row.released_at.is_none());
}

#[test]
fn ratchet_severity_floor_monotone_max() {
    let (_dir, db) = setup_db();
    db.upsert_constraint_ratchet("abc12345", None, "desc", "advisory")
        .unwrap();
    assert_eq!(
        db.get_constraint_ratchet("abc12345")
            .unwrap()
            .unwrap()
            .severity_floor,
        "advisory"
    );

    // Increase to blocking — floor should rise
    db.upsert_constraint_ratchet("abc12345", None, "desc", "blocking")
        .unwrap();
    assert_eq!(
        db.get_constraint_ratchet("abc12345")
            .unwrap()
            .unwrap()
            .severity_floor,
        "blocking"
    );

    // Try to lower to informational — floor must NOT drop
    db.upsert_constraint_ratchet("abc12345", None, "desc", "informational")
        .unwrap();
    assert_eq!(
        db.get_constraint_ratchet("abc12345")
            .unwrap()
            .unwrap()
            .severity_floor,
        "blocking"
    );
}

#[test]
fn ratchet_reregistration_idempotent() {
    let (_dir, db) = setup_db();
    db.upsert_constraint_ratchet("abc12345", Some("rule"), "desc", "blocking")
        .unwrap();
    let first = db.get_constraint_ratchet("abc12345").unwrap().unwrap();

    db.upsert_constraint_ratchet("abc12345", Some("rule"), "desc", "blocking")
        .unwrap();
    let second = db.get_constraint_ratchet("abc12345").unwrap().unwrap();

    assert_eq!(first.id, second.id);
    assert_eq!(first.severity_floor, second.severity_floor);
}

#[test]
fn ratchet_release_and_list_active() {
    let (_dir, db) = setup_db();
    db.upsert_constraint_ratchet("aaa11111", None, "desc1", "blocking")
        .unwrap();
    db.upsert_constraint_ratchet("bbb22222", None, "desc2", "advisory")
        .unwrap();

    assert_eq!(db.get_active_constraint_ratchets().unwrap().len(), 2);

    let released = db
        .release_constraint_ratchet("aaa11111", "josh", "no longer needed")
        .unwrap();
    assert!(released);

    let active = db.get_active_constraint_ratchets().unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(&*active[0].constraint_id, "bbb22222");

    // Released row still in DB with audit trail
    let row = db.get_constraint_ratchet("aaa11111").unwrap().unwrap();
    assert!(row.released_at.is_some());
    assert_eq!(row.released_by.as_deref(), Some("josh"));
    assert_eq!(row.release_rationale.as_deref(), Some("no longer needed"));
}

#[test]
fn ratchet_flag_removal_keeps_row() {
    let (_dir, db) = setup_db();
    // Register a ratchet
    db.upsert_constraint_ratchet("abc12345", Some("rule"), "desc", "blocking")
        .unwrap();

    // Simulate flag removal by not re-registering — row must persist
    // (registration is append-only; no deletion path exists)
    let row = db.get_constraint_ratchet("abc12345").unwrap();
    assert!(row.is_some(), "ratchet row must survive flag removal");
    assert!(row.unwrap().released_at.is_none());
}

#[test]
fn ratchet_double_release_returns_false() {
    let (_dir, db) = setup_db();
    db.upsert_constraint_ratchet("abc12345", Some("rule"), "desc", "blocking")
        .unwrap();

    assert!(
        db.release_constraint_ratchet("abc12345", "josh", "first release")
            .unwrap()
    );
    assert!(
        !db.release_constraint_ratchet("abc12345", "josh", "second release")
            .unwrap(),
        "double-release must return false"
    );
}

#[test]
fn ratchet_unknown_id_release_returns_false() {
    let (_dir, db) = setup_db();
    assert!(
        !db.release_constraint_ratchet("nonexistent", "josh", "reason")
            .unwrap(),
        "releasing unknown id must return false"
    );
}

#[test]
fn ratchet_get_all_includes_released() {
    let (_dir, db) = setup_db();
    db.upsert_constraint_ratchet("aaa11111", None, "desc1", "blocking")
        .unwrap();
    db.upsert_constraint_ratchet("bbb22222", None, "desc2", "advisory")
        .unwrap();

    db.release_constraint_ratchet("aaa11111", "josh", "done")
        .unwrap();

    let all = db.get_all_constraint_ratchets().unwrap();
    assert_eq!(all.len(), 2, "get_all must include released rows");

    let released: Vec<_> = all.iter().filter(|r| r.released_at.is_some()).collect();
    assert_eq!(released.len(), 1);
    assert_eq!(&*released[0].constraint_id, "aaa11111");

    let active: Vec<_> = all.iter().filter(|r| r.released_at.is_none()).collect();
    assert_eq!(active.len(), 1);
    assert_eq!(&*active[0].constraint_id, "bbb22222");
}

#[test]
fn ratchet_reregistration_after_release_reactivates() {
    let (_dir, db) = setup_db();
    db.upsert_constraint_ratchet("abc12345", Some("rule"), "desc", "blocking")
        .unwrap();
    db.release_constraint_ratchet("abc12345", "josh", "no longer needed")
        .unwrap();

    let active = db.get_active_constraint_ratchets().unwrap();
    assert_eq!(active.len(), 0, "released ratchet must not be active");

    db.upsert_constraint_ratchet("abc12345", Some("rule"), "desc v2", "advisory")
        .unwrap();

    let active = db.get_active_constraint_ratchets().unwrap();
    assert_eq!(active.len(), 1, "re-registered ratchet must be active");
    let row = &active[0];
    assert_eq!(&*row.constraint_id, "abc12345");
    assert!(row.released_at.is_none());
    assert!(row.released_by.is_none());
    assert!(row.release_rationale.is_none());
    assert_eq!(
        row.severity_floor, "advisory",
        "re-registration starts fresh severity floor"
    );
}
