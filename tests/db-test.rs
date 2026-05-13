use std::collections::HashMap;

use sutra::db::{Db, InsertSymbolParams, SnapshotParams};

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("test", dir.path()).unwrap();
    (dir, db)
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
        flags: 0,
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
            flags: 0,
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
        flags: 0,
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
    assert_eq!(count, 4, "fresh DB should register all 4 existing migrations");
}

#[test]
fn test_migration_reopen_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let _db1 = Db::open("test", dir.path()).unwrap();
    drop(_db1);
    let db2 = Db::open("test", dir.path()).unwrap();
    let conn = db2.conn_for_test();
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 4, "reopen should not duplicate migration rows");
}

#[test]
fn test_migration_hash_mismatch_errors() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = Db::open("test", dir.path()).unwrap();
        let conn = db.conn_for_test();
        conn.execute(
            "UPDATE schema_migrations SET content_hash = 'tampered' WHERE name = '0001_initial'",
            [],
        )
        .unwrap();
    }
    let result = Db::open("test", dir.path());
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
    db.upsert_convention("abc123", "kind:function", "has_sig", 42, 0.95)
        .unwrap();
    let rows = db.all_conventions().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, "abc123");
    assert_eq!(rows[0].antecedent, "kind:function");
    assert_eq!(rows[0].consequent, "has_sig");
    assert_eq!(rows[0].support, 42);
    assert!((rows[0].confidence - 0.95).abs() < 1e-9);
    assert!(!rows[0].suppressed);
}

#[test]
fn convention_upsert_updates_existing() {
    let (_dir, db) = setup_db();
    db.upsert_convention("abc123", "kind:function", "has_sig", 42, 0.95)
        .unwrap();
    db.upsert_convention("abc123", "kind:function", "has_sig", 50, 0.97)
        .unwrap();
    let rows = db.all_conventions().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].support, 50);
    assert!((rows[0].confidence - 0.97).abs() < 1e-9);
}

#[test]
fn convention_suppress() {
    let (_dir, db) = setup_db();
    db.upsert_convention("abc123", "kind:function", "has_sig", 42, 0.95)
        .unwrap();
    db.suppress_convention("abc123", true).unwrap();
    let rows = db.all_conventions().unwrap();
    assert!(rows[0].suppressed);
    db.suppress_convention("abc123", false).unwrap();
    let rows = db.all_conventions().unwrap();
    assert!(!rows[0].suppressed);
}

#[test]
fn convention_delete_stale() {
    let (_dir, db) = setup_db();
    db.upsert_convention("aaa", "a", "b", 10, 0.9).unwrap();
    db.upsert_convention("bbb", "c", "d", 20, 0.95).unwrap();
    db.upsert_convention("ccc", "e", "f", 30, 0.99).unwrap();
    let deleted = db.delete_stale_conventions(&["aaa", "ccc"]).unwrap();
    assert_eq!(deleted, 1);
    let rows = db.all_conventions().unwrap();
    assert_eq!(rows.len(), 2);
    let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"aaa"));
    assert!(ids.contains(&"ccc"));
}
