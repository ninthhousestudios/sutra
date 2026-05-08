use sutra::db::{Db, InsertSymbolParams};
use sutra::tools::trace;

fn sym<'a>(
    file_id: i64,
    qn: &'a str,
    sn: &'a str,
    kind: &'a str,
    sl: i64,
    el: i64,
) -> InsertSymbolParams<'a> {
    InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind,
        signature: None,
        signature_hash: None,
        visibility: Some("pub"),
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

#[test]
fn entry_point_detection() {
    assert!(trace::is_known_entry_point("main", "function"));
    assert!(trace::is_known_entry_point("build", "method"));
    assert!(trace::is_known_entry_point("initState", "method"));
    assert!(trace::is_known_entry_point("dispose", "method"));

    assert!(!trace::is_known_entry_point("build", "function"));
    assert!(!trace::is_known_entry_point("helper", "function"));
    assert!(!trace::is_known_entry_point("process", "method"));
}

// Linear chain: main → process → helper
// Forward from helper should yield [main, process, helper]
// Backward from main should yield [main, process, helper]
fn setup_linear_chain() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("trace_test", dir.path()).unwrap();

    db.upsert_file("src/main.rs", "rust", "h1", 100, true).unwrap();
    db.upsert_file("src/process.rs", "rust", "h2", 50, true).unwrap();
    db.upsert_file("src/helper.rs", "rust", "h3", 30, true).unwrap();

    let f1 = db.file_by_path("src/main.rs").unwrap().unwrap();
    let f2 = db.file_by_path("src/process.rs").unwrap().unwrap();
    let f3 = db.file_by_path("src/helper.rs").unwrap().unwrap();

    // main calls process (line 10), process calls helper (line 5)
    db.insert_symbol(&sym(f1.id, "main", "main", "function", 1, 20)).unwrap();
    db.insert_symbol(&sym(f2.id, "mod::process", "process", "function", 1, 15)).unwrap();
    db.insert_symbol(&sym(f3.id, "mod::helper", "helper", "function", 1, 10)).unwrap();

    let process_sym = db.symbol_by_qualified_name("mod::process").unwrap().unwrap();
    let helper_sym = db.symbol_by_qualified_name("mod::helper").unwrap().unwrap();

    // main calls process at line 10
    db.insert_ref(f1.id, Some(process_sym.id), None, 10, 0, "call").unwrap();
    // process calls helper at line 5
    db.insert_ref(f2.id, Some(helper_sym.id), None, 5, 0, "call").unwrap();

    (dir, db)
}

#[test]
fn forward_trace_linear_chain() {
    let (_dir, db) = setup_linear_chain();
    let result = trace::handle(&db, "mod::helper", Some("forward"), None).unwrap();

    let paths = result["paths"].as_array().unwrap();
    assert!(!paths.is_empty(), "should find at least one path");

    let chain = paths[0]["chain"].as_array().unwrap();
    let names: Vec<&str> = chain.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(names, vec!["main", "mod::process", "mod::helper"]);
    assert_eq!(paths[0]["has_cycle"], false);
    assert_eq!(paths[0]["reaches_entry_point"], true);
}

#[test]
fn backward_trace_linear_chain() {
    let (_dir, db) = setup_linear_chain();
    let result = trace::handle(&db, "main", Some("backward"), None).unwrap();

    let paths = result["paths"].as_array().unwrap();
    assert!(!paths.is_empty(), "should find at least one path");

    let chain = paths[0]["chain"].as_array().unwrap();
    let names: Vec<&str> = chain.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(names, vec!["main", "mod::process", "mod::helper"]);
    assert_eq!(paths[0]["has_cycle"], false);
    assert_eq!(paths[0]["is_leaf"], true);
}

#[test]
fn cycle_detected_and_marked() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("cycle_test", dir.path()).unwrap();

    db.upsert_file("src/a.rs", "rust", "ha", 50, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 50, true).unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();

    db.insert_symbol(&sym(fa.id, "mod::alpha", "alpha", "function", 1, 20)).unwrap();
    db.insert_symbol(&sym(fb.id, "mod::beta", "beta", "function", 1, 20)).unwrap();

    let alpha = db.symbol_by_qualified_name("mod::alpha").unwrap().unwrap();
    let beta = db.symbol_by_qualified_name("mod::beta").unwrap().unwrap();

    // alpha calls beta at line 5, beta calls alpha at line 5
    db.insert_ref(fa.id, Some(beta.id), None, 5, 0, "call").unwrap();
    db.insert_ref(fb.id, Some(alpha.id), None, 5, 0, "call").unwrap();

    let result = trace::handle(&db, "mod::alpha", Some("backward"), None).unwrap();
    let paths = result["paths"].as_array().unwrap();

    let has_any_cycle = paths.iter().any(|p| p["has_cycle"].as_bool() == Some(true));
    assert!(has_any_cycle, "cycle between alpha and beta should be detected");

    let cycle_path = paths.iter().find(|p| p["has_cycle"].as_bool() == Some(true)).unwrap();
    assert!(cycle_path["cycle_at"].as_str().is_some(), "cycle_at should name the cycle target");
}

#[test]
fn path_limit_respected() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("limit_test", dir.path()).unwrap();

    // Create a fan-in: target called by many independent callers
    db.upsert_file("src/target.rs", "rust", "ht", 20, true).unwrap();
    let ft = db.file_by_path("src/target.rs").unwrap().unwrap();
    db.insert_symbol(&sym(ft.id, "mod::target", "target", "function", 1, 10)).unwrap();
    let target = db.symbol_by_qualified_name("mod::target").unwrap().unwrap();

    for i in 0..20 {
        let path = format!("src/caller_{i}.rs");
        db.upsert_file(&path, "rust", &format!("hc{i}"), 20, true).unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("mod::caller_{i}");
        let sn = format!("caller_{i}");
        db.insert_symbol(&sym(f.id, &qn, &sn, "function", 1, 10)).unwrap();
        db.insert_ref(f.id, Some(target.id), None, 5, 0, "call").unwrap();
    }

    let result = trace::handle(&db, "mod::target", Some("forward"), Some(3)).unwrap();
    let paths = result["paths"].as_array().unwrap();
    assert!(paths.len() <= 3, "should respect limit=3, got {}", paths.len());
    assert_eq!(result["limit"], 3);
    assert_eq!(result["truncated"], true);
}

#[test]
fn entry_point_rules_documented() {
    let (_dir, db) = setup_linear_chain();
    let result = trace::handle(&db, "mod::helper", Some("forward"), None).unwrap();

    let rules = &result["entry_point_rules"];
    assert!(rules["name_based"]["rust"].as_array().is_some());
    assert!(rules["name_based"]["dart"].as_array().is_some());
    assert!(rules["structural"].as_str().is_some());
    assert!(rules["default_limit"].as_u64().is_some());
}
