use std::collections::HashMap;

use sutra::db::{Db, InsertSymbolParams, SnapshotParams, TablePartition, TABLE_REGISTRY};
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
    db.insert_import(fid, "std::io", None, 1).unwrap();

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
    assert_eq!(by_id.qualified_name, "lib::bar");
    assert_eq!(by_id.short_name, "bar");
    assert_eq!(by_id.kind, "function");
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
    assert_eq!(results[0].short_name, "alpha");
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
    assert_eq!(fns[0].kind, "function");

    let structs = db
        .find_symbols_by_name("process", Some("struct"), 10)
        .unwrap();
    assert_eq!(structs.len(), 1);
    assert_eq!(structs[0].kind, "struct");
}

#[test]
fn test_find_symbols_by_name_fts_fallback() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_symbol(&db, fid, "lib::my_function", "my_function", "function");

    let results = db.find_symbols_by_name("my_func", None, 10).unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].short_name, "my_function");
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

    let ids: Vec<i64> = summary.iter().map(|(id, _, _, _)| *id).collect();
    assert!(ids.contains(&id1));
    assert!(ids.contains(&id2));
    assert!(ids.contains(&id3));

    let qnames: Vec<&str> = summary.iter().map(|(_, qn, _, _)| qn.as_str()).collect();
    assert!(qnames.contains(&"lib::alpha"));

    let snames: Vec<&str> = summary.iter().map(|(_, _, sn, _)| sn.as_str()).collect();
    assert!(snames.contains(&"beta"));

    let kinds: Vec<&str> = summary.iter().map(|(_, _, _, k)| k.as_str()).collect();
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

    db.insert_import(fid, "std::io", None, 1).unwrap();
    db.insert_import(fid, "std::fmt", None, 2).unwrap();

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

    db.insert_import(fa, "b", Some(fb), 1).unwrap();
    db.insert_import(fa, "c", Some(fc), 2).unwrap();
    db.insert_import(fb, "unresolved", None, 1).unwrap();

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
        health_score: 0,
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
        health_score: 78,
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
    assert_eq!(s.health_score, 78);
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
        health_score: 90,
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
        health_score: 75,
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
        health_score: 90,
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
        health_score: 75,
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
        health_score: 90,
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
        health_score: 75,
    })
    .unwrap();

    let result = sutra::tools::trend::handle(&db, None, None).unwrap();
    let deltas = &result["deltas"];
    assert_eq!(deltas["files_parsed"], 10);
    assert_eq!(deltas["symbols_extracted"], 30);
    assert_eq!(deltas["total_complexity"], 15);
    assert_eq!(deltas["dead_symbol_count"], 3);
    assert_eq!(deltas["hotspot_count"], 2);
    assert_eq!(deltas["health_score"], -15);
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
        health_score: 90,
    })
    .unwrap();
    let result = sutra::tools::trend::handle(&db, None, None);
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
        health_score: 0,
    })
    .unwrap();
    let snaps = db.latest_snapshots(1).unwrap();
    assert_eq!(snaps[0].total_complexity, 0);
    assert_eq!(snaps[0].dead_symbol_count, 0);
    assert_eq!(snaps[0].hotspot_count, 0);
    assert_eq!(snaps[0].health_score, 0);
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
    assert!(results.is_empty() || results[0].qualified_name == "foo::bar");

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
    assert_eq!(
        count, 28,
        "fresh DB should register all 28 existing migrations"
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
    assert_eq!(count, 28, "reopen should not duplicate migration rows");
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
fn test_set_convention_lifecycle() {
    let (_dir, db) = setup_db();
    db.upsert_convention("abc123", "kind:function", "has_sig", 42, 0.95, None)
        .unwrap();

    db.set_convention_lifecycle("abc123", "forbidden", Some("bad pattern"))
        .unwrap();
    let ov = db.get_convention_state("abc123").unwrap().unwrap();
    assert_eq!(ov.lifecycle_state, "forbidden");
    assert_eq!(ov.override_reason.as_deref(), Some("bad pattern"));

    db.set_convention_lifecycle("abc123", "preferred", None)
        .unwrap();
    let ov = db.get_convention_state("abc123").unwrap().unwrap();
    assert_eq!(ov.lifecycle_state, "preferred");
    assert!(ov.override_reason.is_none());
}

#[test]
fn test_all_conventions_merged() {
    let (_dir, db) = setup_db();
    db.upsert_convention("aaa", "a", "b", 10, 0.9, None).unwrap();
    db.upsert_convention("bbb", "c", "d", 20, 0.95, None).unwrap();
    db.set_convention_lifecycle("aaa", "deprecated", Some("outdated"))
        .unwrap();

    let merged = db.all_conventions_merged().unwrap();
    assert_eq!(merged.len(), 2);

    let aaa = merged.iter().find(|c| c.id == "aaa").unwrap();
    assert_eq!(aaa.lifecycle_state.as_deref(), Some("deprecated"));

    let bbb = merged.iter().find(|c| c.id == "bbb").unwrap();
    assert!(bbb.lifecycle_state.is_none());
}

#[test]
fn test_reconcile_orphaned_states() {
    let (_dir, db) = setup_db();
    db.set_convention_lifecycle("nonexistent", "forbidden", Some("orphan"))
        .unwrap();

    let orphans = db.reconcile_orphaned_states().unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].convention_id, "nonexistent");

    db.upsert_convention("nonexistent", "x", "y", 5, 0.8, None)
        .unwrap();
    let orphans = db.reconcile_orphaned_states().unwrap();
    assert!(orphans.is_empty());
}

#[test]
fn test_reindex_preserves_durable_state() {
    let (_dir, db) = setup_db();

    db.upsert_convention("conv1", "kind:function", "has_sig", 10, 0.9, None)
        .unwrap();
    db.set_convention_lifecycle("conv1", "preferred", Some("team standard"))
        .unwrap();

    let dropped = db.reindex().unwrap();
    assert!(!dropped.contains(&"convention_state"));

    let conventions = db.all_conventions().unwrap();
    assert!(conventions.is_empty(), "ephemeral conventions should be gone");

    let ov = db.get_convention_state("conv1").unwrap().unwrap();
    assert_eq!(ov.lifecycle_state, "preferred");
    assert_eq!(ov.override_reason.as_deref(), Some("team standard"));

    let orphans = db.reconcile_orphaned_states().unwrap();
    assert_eq!(orphans.len(), 1, "override without convention is orphaned");

    db.upsert_convention("conv1", "kind:function", "has_sig", 12, 0.92, None)
        .unwrap();
    let merged = db.all_conventions_merged().unwrap();
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].lifecycle_state.as_deref(), Some("preferred"));
}

#[test]
fn convention_delete_stale() {
    let (_dir, db) = setup_db();
    db.upsert_convention("aaa", "a", "b", 10, 0.9, None).unwrap();
    db.upsert_convention("bbb", "c", "d", 20, 0.95, None).unwrap();
    db.upsert_convention("ccc", "e", "f", 30, 0.99, None).unwrap();
    let deleted = db.delete_stale_conventions(&["aaa", "ccc"]).unwrap();
    assert_eq!(deleted, 1);
    let rows = db.all_conventions().unwrap();
    assert_eq!(rows.len(), 2);
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"aaa"));
    assert!(ids.contains(&"ccc"));
}

#[test]
fn convention_delete_stale_preserves_stateful() {
    let (_dir, db) = setup_db();
    db.upsert_convention("aaa", "a", "b", 10, 0.9, None).unwrap();
    db.upsert_convention("bbb", "c", "d", 20, 0.95, None).unwrap();
    db.upsert_convention("ccc", "e", "f", 30, 0.99, None).unwrap();
    db.set_convention_lifecycle("bbb", "deprecated", Some("test")).unwrap();

    // bbb is not in current_ids but has convention_state → should survive
    let deleted = db.delete_stale_conventions(&["aaa"]).unwrap();
    assert_eq!(deleted, 1); // only ccc deleted
    let rows = db.all_conventions().unwrap();
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"aaa"));
    assert!(ids.contains(&"bbb"), "stateful convention should survive deletion");
    assert!(!ids.contains(&"ccc"));
}

#[test]
fn tracked_absent_conventions_identified() {
    let (_dir, db) = setup_db();
    db.upsert_convention("aaa", "a", "b", 10, 0.9, None).unwrap();
    db.upsert_convention("bbb", "c", "d", 20, 0.95, None).unwrap();
    db.set_convention_lifecycle("bbb", "deprecated", Some("test")).unwrap();

    let absent = db.tracked_convention_ids_absent_from(&["aaa"]).unwrap();
    assert_eq!(absent, vec!["bbb"]);

    let absent = db.tracked_convention_ids_absent_from(&["aaa", "bbb"]).unwrap();
    assert!(absent.is_empty());
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
    assert!(names.contains(&"convention_state"));

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
    assert!(files.is_empty(), "ephemeral files table should be empty after reindex");
    let conventions = db.all_conventions().unwrap();
    assert!(conventions.is_empty(), "ephemeral conventions table should be empty after reindex");

    let file_id = seed_file(&db, "src/lib.rs");
    assert!(file_id > 0, "should be able to insert after reindex");
    seed_symbol(&db, file_id, "lib_fn", "lib_fn", "function");
    db.upsert_convention("conv2", "kind:struct", "has_doc", 5, 0.8, None)
        .unwrap();
}

// --- Convention waiver tests ---

#[test]
fn waiver_create_list_revoke() {
    let (_dir, db) = setup_db();

    let id = db
        .create_waiver("conv1", "process", "", "intentional deviation", "josh")
        .unwrap();
    assert!(id > 0);

    let waivers = db.list_waivers(None).unwrap();
    assert_eq!(waivers.len(), 1);
    assert_eq!(waivers[0].convention_id, "conv1");
    assert_eq!(waivers[0].symbol_qualified_name, "process");
    assert_eq!(waivers[0].rationale, "intentional deviation");
    assert_eq!(waivers[0].waived_by, "josh");

    let revoked = db.revoke_waiver(id).unwrap();
    assert!(revoked);

    let waivers = db.list_waivers(None).unwrap();
    assert!(waivers.is_empty());

    let not_found = db.revoke_waiver(id).unwrap();
    assert!(!not_found);
}

#[test]
fn waiver_uniqueness_upserts() {
    let (_dir, db) = setup_db();

    db.create_waiver("conv1", "process", "", "first reason", "alice")
        .unwrap();
    db.create_waiver("conv1", "process", "", "updated reason", "bob")
        .unwrap();

    let waivers = db.list_waivers(None).unwrap();
    assert_eq!(waivers.len(), 1);
    assert_eq!(waivers[0].rationale, "updated reason");
    assert_eq!(waivers[0].waived_by, "bob");
}

#[test]
fn waiver_component_scoping() {
    let (_dir, db) = setup_db();

    db.create_waiver("conv1", "process", "", "global waiver", "josh")
        .unwrap();
    db.create_waiver("conv1", "process", "comp_a", "scoped waiver", "josh")
        .unwrap();

    let all = db.list_waivers(None).unwrap();
    assert_eq!(all.len(), 2);

    let by_conv = db.list_waivers(Some("conv1")).unwrap();
    assert_eq!(by_conv.len(), 2);

    let check_map = db.waivers_for_check().unwrap();
    assert_eq!(check_map.len(), 2);
    assert_eq!(
        check_map.get(&("conv1".into(), "process".into(), "".into())),
        Some(&"global waiver".to_string())
    );
    assert_eq!(
        check_map.get(&("conv1".into(), "process".into(), "comp_a".into())),
        Some(&"scoped waiver".to_string())
    );
}

#[test]
fn waiver_orphan_detection() {
    let (_dir, db) = setup_db();

    db.create_waiver("orphan_conv", "some_sym", "", "will be orphaned", "josh")
        .unwrap();

    // Convention doesn't exist → orphaned
    let orphans = db.reconcile_orphaned_waivers().unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].convention_id, "orphan_conv");

    // Convention exists but symbol doesn't → still orphaned (symbol-orphaned)
    db.upsert_convention("orphan_conv", "a", "b", 10, 0.9, None)
        .unwrap();
    let orphans = db.reconcile_orphaned_waivers().unwrap();
    assert_eq!(orphans.len(), 1, "waiver with nonexistent symbol should be orphaned");

    // Add a matching symbol → no longer orphaned
    let file_id = seed_file(&db, "src/lib.rs");
    seed_symbol(&db, file_id, "some_sym", "some_sym", "function");
    let orphans = db.reconcile_orphaned_waivers().unwrap();
    assert!(orphans.is_empty(), "waiver with existing symbol should not be orphaned");
}

#[test]
fn waiver_orphan_detection_file_qualified() {
    let (_dir, db) = setup_db();

    db.upsert_convention("conv1", "a", "b", 10, 0.9, None).unwrap();
    let file_id = seed_file(&db, "src/foo.rs");
    seed_symbol(&db, file_id, "process", "process", "function");

    // File-qualified waiver matching existing file+symbol → not orphaned
    db.create_waiver("conv1", "src/foo.rs::process", "", "file scoped", "josh")
        .unwrap();
    let orphans = db.reconcile_orphaned_waivers().unwrap();
    assert!(orphans.is_empty(), "file-qualified waiver with matching file+symbol should not be orphaned");

    // File-qualified waiver with wrong file → orphaned
    db.create_waiver("conv1", "src/bar.rs::process", "", "wrong file", "josh")
        .unwrap();
    let orphans = db.reconcile_orphaned_waivers().unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].symbol_qualified_name, "src/bar.rs::process");
}

#[test]
fn waiver_survives_reindex() {
    let (_dir, db) = setup_db();

    db.upsert_convention("conv1", "kind:function", "has_sig", 10, 0.9, None)
        .unwrap();
    db.create_waiver("conv1", "process", "", "intentional", "josh")
        .unwrap();

    let dropped = db.reindex().unwrap();
    assert!(!dropped.contains(&"convention_waivers"));

    let waivers = db.list_waivers(None).unwrap();
    assert_eq!(waivers.len(), 1);
    assert_eq!(waivers[0].convention_id, "conv1");
    assert_eq!(waivers[0].rationale, "intentional");

    let orphans = db.reconcile_orphaned_waivers().unwrap();
    assert_eq!(orphans.len(), 1, "waiver is orphaned after reindex clears conventions");
}

// --- Constraint waiver tests ---

#[test]
fn constraint_waiver_crud() {
    let (_dir, db) = setup_db();

    let id = db
        .create_constraint_waiver("abc12345", Some("no-tool-daemon"), "src/tools/review.rs", None, "legacy code", "josh")
        .unwrap();
    assert!(id > 0);

    let waivers = db.get_constraint_waivers(None).unwrap();
    assert_eq!(waivers.len(), 1);
    assert_eq!(waivers[0].constraint_id, "abc12345");
    assert_eq!(waivers[0].constraint_name.as_deref(), Some("no-tool-daemon"));
    assert_eq!(waivers[0].file_path, "src/tools/review.rs");
    assert!(waivers[0].symbol_qualified_name.is_none());
    assert_eq!(waivers[0].rationale, "legacy code");
    assert_eq!(waivers[0].waived_by, "josh");

    let filtered = db.get_constraint_waivers(Some("abc12345")).unwrap();
    assert_eq!(filtered.len(), 1);
    let empty = db.get_constraint_waivers(Some("nonexistent")).unwrap();
    assert!(empty.is_empty());

    let updated = db.update_constraint_waiver(id, "approved exception").unwrap();
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
        .create_constraint_waiver("abc", Some("named"), "src/a.rs", None, "updated reason", "bob")
        .unwrap();

    assert_eq!(first_id, second_id, "upsert must return the existing row's id");

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
    assert_eq!(for_b[0].constraint_id, "abc");

    let for_c = db.get_constraint_waivers_for_file("src/c.rs").unwrap();
    assert!(for_c.is_empty());
}

#[test]
fn constraint_waiver_symbol_scoping() {
    let (_dir, db) = setup_db();

    db.create_constraint_waiver("abc", None, "src/a.rs", None, "file-level", "josh")
        .unwrap();
    db.create_constraint_waiver("abc", None, "src/a.rs", Some("Foo::bar"), "symbol-level", "josh")
        .unwrap();

    let waivers = db.get_constraint_waivers(Some("abc")).unwrap();
    assert_eq!(waivers.len(), 2);

    // Two NULL-symbol waivers for different constraints are distinct
    db.create_constraint_waiver("def", None, "src/a.rs", None, "different constraint", "josh")
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
    assert_eq!(waivers[0].constraint_id, "abc");
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
    let orphan_ids: Vec<&str> = orphans.iter().map(|w| w.constraint_id.as_str()).collect();
    assert!(orphan_ids.contains(&"def"));
    assert!(orphan_ids.contains(&"ghi"));

    // All active → no orphans
    let orphans = db.reconcile_orphaned_constraint_waivers(&["abc", "def", "ghi"]).unwrap();
    assert!(orphans.is_empty());

    // No active → all orphans
    let orphans = db.reconcile_orphaned_constraint_waivers(&[]).unwrap();
    assert_eq!(orphans.len(), 3);
}
