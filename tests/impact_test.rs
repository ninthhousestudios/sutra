use std::path::PathBuf;

use sutra::config::Config;
use sutra::db::Db;
use sutra::pipeline;
use sutra::tools::impact;
use sutra::workspace::WorkspaceEntry;

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("test", dir.path()).unwrap();

    // Create files
    db.upsert_file("src/a.rs", "rust", "hash_a", 100, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hash_b", 50, true).unwrap();
    db.upsert_file("src/c.rs", "rust", "hash_c", 30, true).unwrap();

    let file_a = db.file_by_path("src/a.rs").unwrap().unwrap();
    let file_b = db.file_by_path("src/b.rs").unwrap().unwrap();
    let file_c = db.file_by_path("src/c.rs").unwrap().unwrap();

    // Symbol in file A: a function
    db.insert_symbol(
        file_a.id, "a::target_fn", "target_fn", "function",
        Some("fn target_fn()"), None, Some("pub"), 10, 0, 20, 0, None, None,
    ).unwrap();

    let target_sym = db.symbol_by_qualified_name("a::target_fn").unwrap().unwrap();

    // Symbol in file B: calls target_fn
    db.insert_symbol(
        file_b.id, "b::caller_fn", "caller_fn", "function",
        Some("fn caller_fn()"), None, Some("pub"), 5, 0, 15, 0, None, None,
    ).unwrap();

    // Ref from B to target_fn
    db.insert_ref(file_b.id, Some(target_sym.id), None, 10, 0, "call").unwrap();

    // Symbol in file C: calls caller_fn (transitive)
    db.insert_symbol(
        file_c.id, "c::indirect_caller", "indirect_caller", "function",
        Some("fn indirect_caller()"), None, Some("pub"), 5, 0, 15, 0, None, None,
    ).unwrap();

    let caller_sym = db.symbol_by_qualified_name("b::caller_fn").unwrap().unwrap();
    db.insert_ref(file_c.id, Some(caller_sym.id), None, 10, 0, "call").unwrap();

    (dir, db)
}

#[test]
fn test_impact_low_fan_in() {
    let (_dir, db) = setup_db();
    let result = impact::handle(&db, "a::target_fn").unwrap();

    assert_eq!(result["risk"], "low");
    assert_eq!(result["direct_callers"], 1);
}

#[test]
fn test_impact_transitive() {
    let (_dir, db) = setup_db();
    let result = impact::handle(&db, "a::target_fn").unwrap();

    // Transitive BFS should find B and C
    assert!(result["transitive_symbols"].as_u64().unwrap() >= 2);
    assert!(result["files_touched"].as_u64().unwrap() >= 1);
}

#[test]
fn test_impact_unknown_symbol() {
    let (_dir, db) = setup_db();
    let result = impact::handle(&db, "nonexistent::symbol");
    assert!(result.is_err());
}

#[test]
fn test_impact_high_fan_in() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("test_high", dir.path()).unwrap();

    db.upsert_file("src/core.rs", "rust", "hash_core", 200, true).unwrap();
    let core_file = db.file_by_path("src/core.rs").unwrap().unwrap();
    db.insert_symbol(
        core_file.id, "core::hot_fn", "hot_fn", "function",
        Some("fn hot_fn()"), None, Some("pub"), 10, 0, 20, 0, None, None,
    ).unwrap();
    let hot_sym = db.symbol_by_qualified_name("core::hot_fn").unwrap().unwrap();

    // Create 20 files each calling hot_fn
    for i in 0..20 {
        let path = format!("src/caller_{i}.rs");
        db.upsert_file(&path, "rust", &format!("hash_{i}"), 30, true).unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        db.insert_symbol(
            f.id, &format!("caller_{i}::fn_{i}"), &format!("fn_{i}"), "function",
            None, None, None, 1, 0, 10, 0, None, None,
        ).unwrap();
        db.insert_ref(f.id, Some(hot_sym.id), None, 5, 0, "call").unwrap();
    }

    let result = impact::handle(&db, "core::hot_fn").unwrap();
    assert_eq!(result["risk"], "high");
    assert!(result["direct_callers"].as_u64().unwrap() >= 15);
}

#[tokio::test]
async fn test_impact_real_codebase() {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let src_dir = PathBuf::from(manifest_dir).join("src");
    assert!(src_dir.exists(), "sutra src/ must exist for dogfooding test");

    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: "sutra_self".to_string(),
        root: PathBuf::from(manifest_dir),
        languages: vec!["rust".to_string()],
    };
    let config = Config {
        db_dir: db_dir.path().to_path_buf(),
        workspaces_path: db_dir.path().join("workspaces.toml"),
        listen_addr: "127.0.0.1:0".to_string(),
        parse_parallelism: 1,
        stale_threshold_sec: 600,
        log_level: "warn".to_string(),
    };
    let db = Db::open(&ws.id, db_dir.path()).unwrap();

    let snap = pipeline::parse_workspace(&ws, &db, &config).await.unwrap();
    assert!(snap.files_parsed > 0, "expected sutra src/ to contain Rust files");
    assert!(snap.symbols_extracted > 0, "expected symbols from sutra src/");

    // "open" is the most-called method on Db — it must appear in the symbol table.
    let result = impact::handle(&db, "open");
    assert!(result.is_ok(), "impact query on 'open' should succeed against real codebase");
    let val = result.unwrap();
    assert!(val["symbol"].is_string());
    assert!(val["risk"].is_string());
}

#[test]
fn test_fan_in_blast_radius_consistency() {
    // Topology: A → B → C (A calls B, B calls C)
    // fan_in for C: 1 (B references C)
    // fan_in for B: 1 (A references B)
    // fan_in for A: 0 (nobody references A)
    // blast_radius for C (files that depend on C, depth 3): A and B both transitively
    //   depend on C, so blast_radius >= 2.

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("rollup_test", dir.path()).unwrap();

    db.upsert_file("src/a.rs", "rust", "ha", 10, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 10, true).unwrap();
    db.upsert_file("src/c.rs", "rust", "hc", 10, true).unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();
    let fc = db.file_by_path("src/c.rs").unwrap().unwrap();

    db.insert_symbol(fa.id, "a::fn_a", "fn_a", "function", None, None, None, 1, 0, 5, 0, None, None).unwrap();
    db.insert_symbol(fb.id, "b::fn_b", "fn_b", "function", None, None, None, 1, 0, 5, 0, None, None).unwrap();
    db.insert_symbol(fc.id, "c::fn_c", "fn_c", "function", None, None, None, 1, 0, 5, 0, None, None).unwrap();

    let sym_b = db.symbol_by_qualified_name("b::fn_b").unwrap().unwrap();
    let sym_c = db.symbol_by_qualified_name("c::fn_c").unwrap().unwrap();

    // A calls B, B calls C.
    db.insert_ref(fa.id, Some(sym_b.id), None, 3, 0, "call").unwrap();
    db.insert_ref(fb.id, Some(sym_c.id), None, 3, 0, "call").unwrap();

    // Compute rollups manually (same logic as pipeline).
    // fan_in for B = 1 (A references it), for C = 1 (B references it), for A = 0.
    db.update_rollups(fa.id, 0, 0).unwrap();
    db.update_rollups(fb.id, 1, 1).unwrap(); // blast_radius from B's perspective: A depends on B
    db.update_rollups(fc.id, 1, 2).unwrap(); // blast_radius from C's perspective: B and A both depend

    let row_a = db.file_by_path("src/a.rs").unwrap().unwrap();
    let row_b = db.file_by_path("src/b.rs").unwrap().unwrap();
    let row_c = db.file_by_path("src/c.rs").unwrap().unwrap();

    assert_eq!(row_a.fan_in_files, 0, "A has no callers");
    assert_eq!(row_b.fan_in_files, 1, "B is called by A");
    assert_eq!(row_c.fan_in_files, 1, "C is called by B");

    assert_eq!(row_b.blast_radius, 1, "changing B affects A");
    assert_eq!(row_c.blast_radius, 2, "changing C affects B and A");
}
