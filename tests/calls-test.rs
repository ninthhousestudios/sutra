use sutra::db::{Db, InsertSymbolParams};
use sutra::tools::calls;

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

fn sym<'a>(file_id: i64, qn: &'a str, sn: &'a str, sl: i64, el: i64) -> InsertSymbolParams<'a> {
    InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind: "function",
        signature: None,
        signature_hash: None,
        visibility: None,
        start_line: sl,
        start_col: 0,
        end_line: el,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        flags: 0,
    }
}

fn setup_chain() -> (tempfile::TempDir, Db) {
    let (dir, db) = setup_db();

    db.upsert_file("src/a.rs", "rust", "ha", 20, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 20, true).unwrap();
    db.upsert_file("src/c.rs", "rust", "hc", 20, true).unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();
    let fc = db.file_by_path("src/c.rs").unwrap().unwrap();

    db.insert_symbol(&sym(fa.id, "a::fn_a", "fn_a", 1, 20))
        .unwrap();
    db.insert_symbol(&sym(fb.id, "b::fn_b", "fn_b", 1, 20))
        .unwrap();
    db.insert_symbol(&sym(fc.id, "c::fn_c", "fn_c", 1, 20))
        .unwrap();

    let sym_b = db.symbol_by_qualified_name("b::fn_b").unwrap().unwrap();
    let sym_c = db.symbol_by_qualified_name("c::fn_c").unwrap().unwrap();

    db.insert_ref(fa.id, Some(sym_b.id), None, 10, 0, "call")
        .unwrap();
    db.insert_ref(fb.id, Some(sym_c.id), None, 10, 0, "call")
        .unwrap();

    (dir, db)
}

#[test]
fn test_callers_single_depth() {
    let (_dir, db) = setup_chain();

    let result = calls::handle(&db, "b::fn_b", Some("callers"), Some(1)).unwrap();

    assert_eq!(result["symbol"], "b::fn_b");
    assert_eq!(result["direction"], "callers");
    assert_eq!(result["depth"], 1);

    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["caller"], "a::fn_a");
    assert_eq!(entries[0]["depth"], 1);
}

#[test]
fn test_callers_transitive() {
    let (_dir, db) = setup_chain();

    let result = calls::handle(&db, "c::fn_c", Some("callers"), Some(2)).unwrap();

    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);

    let depth1: Vec<_> = entries.iter().filter(|e| e["depth"] == 1).collect();
    let depth2: Vec<_> = entries.iter().filter(|e| e["depth"] == 2).collect();

    assert_eq!(depth1.len(), 1);
    assert_eq!(depth1[0]["caller"], "b::fn_b");

    assert_eq!(depth2.len(), 1);
    assert_eq!(depth2[0]["caller"], "a::fn_a");
}

#[test]
fn test_callers_default_depth() {
    let (_dir, db) = setup_chain();

    let result = calls::handle(&db, "c::fn_c", Some("callers"), None).unwrap();

    assert_eq!(result["depth"], 1);

    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["caller"], "b::fn_b");
    assert_eq!(entries[0]["depth"], 1);
}

#[test]
fn test_callers_depth_capped_at_3() {
    let (_dir, db) = setup_chain();

    let result = calls::handle(&db, "c::fn_c", Some("callers"), Some(10)).unwrap();

    assert_eq!(result["depth"], 3);
}

#[test]
fn test_callees_single() {
    let (_dir, db) = setup_db();

    db.upsert_file("src/a.rs", "rust", "ha", 25, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 10, true).unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();

    db.insert_symbol(&sym(fa.id, "a::fn_a", "fn_a", 1, 20))
        .unwrap();
    db.insert_symbol(&sym(fb.id, "b::fn_b", "fn_b", 1, 10))
        .unwrap();

    let sym_b = db.symbol_by_qualified_name("b::fn_b").unwrap().unwrap();
    db.insert_ref(fa.id, Some(sym_b.id), None, 10, 0, "call")
        .unwrap();

    let result = calls::handle(&db, "a::fn_a", Some("callees"), Some(1)).unwrap();

    assert_eq!(result["direction"], "callees");
    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["callee"], "b::fn_b");
    assert_eq!(entries[0]["depth"], 1);
}

#[test]
fn test_callees_unresolved() {
    let (_dir, db) = setup_db();

    db.upsert_file("src/a.rs", "rust", "ha", 25, true).unwrap();
    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();

    db.insert_symbol(&sym(fa.id, "a::fn_a", "fn_a", 1, 20))
        .unwrap();
    db.insert_ref(fa.id, None, Some("external_fn"), 10, 0, "call")
        .unwrap();

    let result = calls::handle(&db, "a::fn_a", Some("callees"), Some(1)).unwrap();

    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["callee"], "external_fn");
    assert_eq!(entries[0]["unresolved"], true);
}

#[test]
fn test_unknown_symbol_returns_diagnostic() {
    let (_dir, db) = setup_db();

    let result = calls::handle(&db, "nonexistent::fn", Some("callers"), Some(1)).unwrap();
    assert_eq!(result["diagnostic"]["kind"], "no_such_symbol");
    assert_eq!(result["diagnostic"]["queried_name"], "nonexistent::fn");
}

#[test]
fn test_callers_no_callers() {
    let (_dir, db) = setup_db();

    db.upsert_file("src/leaf.rs", "rust", "hl", 10, true)
        .unwrap();
    let fl = db.file_by_path("src/leaf.rs").unwrap().unwrap();
    db.insert_symbol(&sym(fl.id, "leaf::fn_leaf", "fn_leaf", 1, 10))
        .unwrap();

    let result = calls::handle(&db, "leaf::fn_leaf", Some("callers"), Some(1)).unwrap();

    let entries = result["entries"].as_array().unwrap();
    assert!(entries.is_empty());
}

#[test]
fn test_callees_ignores_refs_outside_body() {
    let (_dir, db) = setup_db();

    db.upsert_file("src/a.rs", "rust", "ha", 50, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 10, true).unwrap();
    db.upsert_file("src/c.rs", "rust", "hc", 10, true).unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();
    let fc = db.file_by_path("src/c.rs").unwrap().unwrap();

    db.insert_symbol(&sym(fa.id, "a::fn_a", "fn_a", 5, 15))
        .unwrap();
    db.insert_symbol(&sym(fb.id, "b::fn_b", "fn_b", 1, 10))
        .unwrap();
    db.insert_symbol(&sym(fc.id, "c::fn_c", "fn_c", 1, 10))
        .unwrap();

    let sym_b = db.symbol_by_qualified_name("b::fn_b").unwrap().unwrap();
    let sym_c = db.symbol_by_qualified_name("c::fn_c").unwrap().unwrap();

    db.insert_ref(fa.id, Some(sym_b.id), None, 10, 0, "call")
        .unwrap();
    db.insert_ref(fa.id, Some(sym_c.id), None, 30, 0, "call")
        .unwrap();

    let result = calls::handle(&db, "a::fn_a", Some("callees"), Some(1)).unwrap();

    let entries = result["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["callee"], "b::fn_b");
}

#[test]
fn test_callers_cycle_detection() {
    let (_dir, db) = setup_db();

    db.upsert_file("src/a.rs", "rust", "ha", 20, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 20, true).unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();

    db.insert_symbol(&sym(fa.id, "a::fn_a", "fn_a", 1, 20))
        .unwrap();
    db.insert_symbol(&sym(fb.id, "b::fn_b", "fn_b", 1, 20))
        .unwrap();

    let sym_a = db.symbol_by_qualified_name("a::fn_a").unwrap().unwrap();
    let sym_b = db.symbol_by_qualified_name("b::fn_b").unwrap().unwrap();

    db.insert_ref(fa.id, Some(sym_b.id), None, 10, 0, "call")
        .unwrap();
    db.insert_ref(fb.id, Some(sym_a.id), None, 10, 0, "call")
        .unwrap();

    let result = calls::handle(&db, "a::fn_a", Some("callers"), Some(3)).unwrap();
    assert!(result["entries"].is_array());
}
