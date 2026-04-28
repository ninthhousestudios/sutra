use sutra::db::Db;
use sutra::tools::impact;

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
