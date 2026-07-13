use std::path::Path;

use sutra::db::{Db, InsertSymbolParams};
use sutra::tools::context;

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

fn sym<'a>(
    file_id: i64,
    qn: &'a str,
    sn: &'a str,
    kind: &'a str,
    sig: Option<&'a str>,
    sl: i64,
    el: i64,
) -> InsertSymbolParams<'a> {
    InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind,
        signature: sig,
        signature_hash: None,
        structural_hash: None,
        visibility: None,
        start_line: sl,
        start_col: 0,
        end_line: el,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    }
}

fn write_file(workspace: &Path, rel_path: &str, content: &str) {
    let abs = workspace.join(rel_path);
    std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
    std::fs::write(abs, content).unwrap();
}

/// Build a small workspace: target fn_a calls fn_b and fn_c; fn_d calls fn_a.
fn setup_workspace() -> (tempfile::TempDir, Db) {
    let (dir, db) = setup_db();
    let workspace = dir.path();

    let src_a = "\
fn fn_a() -> i32 {
    let x = fn_b();
    let y = fn_c();
    x + y
}
";
    let src_b = "\
fn fn_b() -> i32 {
    42
}
";
    let src_c = "\
fn fn_c() -> i32 {
    99
}
";
    let src_d = "\
fn fn_d() -> i32 {
    fn_a()
}
";
    let src_test = "\
#[test]
fn test_fn_a() {
    assert_eq!(fn_a(), 141);
}
";

    write_file(workspace, "src/a.rs", src_a);
    write_file(workspace, "src/b.rs", src_b);
    write_file(workspace, "src/c.rs", src_c);
    write_file(workspace, "src/d.rs", src_d);
    write_file(workspace, "tests/a_test.rs", src_test);

    db.upsert_file("src/a.rs", "rust", "ha", 5, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 3, true).unwrap();
    db.upsert_file("src/c.rs", "rust", "hc", 3, true).unwrap();
    db.upsert_file("src/d.rs", "rust", "hd", 3, true).unwrap();
    db.upsert_file("tests/a_test.rs", "rust", "ht", 4, true)
        .unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();
    let fc = db.file_by_path("src/c.rs").unwrap().unwrap();
    let fd = db.file_by_path("src/d.rs").unwrap().unwrap();
    let ft = db.file_by_path("tests/a_test.rs").unwrap().unwrap();

    db.insert_symbol(&sym(
        fa.id,
        "fn_a",
        "fn_a",
        "function",
        Some("fn fn_a() -> i32"),
        1,
        5,
    ))
    .unwrap();
    db.insert_symbol(&sym(
        fb.id,
        "fn_b",
        "fn_b",
        "function",
        Some("fn fn_b() -> i32"),
        1,
        3,
    ))
    .unwrap();
    db.insert_symbol(&sym(
        fc.id,
        "fn_c",
        "fn_c",
        "function",
        Some("fn fn_c() -> i32"),
        1,
        3,
    ))
    .unwrap();
    db.insert_symbol(&sym(
        fd.id,
        "fn_d",
        "fn_d",
        "function",
        Some("fn fn_d() -> i32"),
        1,
        3,
    ))
    .unwrap();
    db.insert_symbol(&sym(
        ft.id,
        "test_fn_a",
        "test_fn_a",
        "function",
        Some("fn test_fn_a()"),
        1,
        4,
    ))
    .unwrap();

    let sym_b = db.symbol_by_qualified_name("fn_b").unwrap().unwrap();
    let sym_c = db.symbol_by_qualified_name("fn_c").unwrap().unwrap();
    let sym_a = db.symbol_by_qualified_name("fn_a").unwrap().unwrap();

    // fn_a calls fn_b (line 2) and fn_c (line 3)
    db.insert_ref(fa.id, Some(sym_b.id), None, 2, 0, "call")
        .unwrap();
    db.insert_ref(fa.id, Some(sym_c.id), None, 3, 0, "call")
        .unwrap();

    // fn_d calls fn_a (line 2)
    db.insert_ref(fd.id, Some(sym_a.id), None, 2, 0, "call")
        .unwrap();

    // test_fn_a calls fn_a (line 3)
    db.insert_ref(ft.id, Some(sym_a.id), None, 3, 0, "call")
        .unwrap();

    (dir, db)
}

#[test]
fn full_context_within_budget() {
    let (dir, db) = setup_workspace();
    let result =
        context::handle(&db, dir.path(), "fn_a", Some(10000), Some(2), false, None).unwrap();

    assert_eq!(result["symbol"], "fn_a");
    assert_eq!(result["file"], "src/a.rs");
    assert_eq!(result["target_omitted"], false);
    assert_eq!(result["truncated"], false);

    let ctx = result["context"].as_array().unwrap();
    assert!(!ctx.is_empty());

    // Target should be first
    assert_eq!(ctx[0]["role"], "target");
    assert_eq!(ctx[0]["symbol"], "fn_a");
    let target_content = ctx[0]["content"].as_str().unwrap();
    assert!(target_content.contains("fn fn_a()"));

    // Should have direct dependencies (fn_b, fn_c)
    let dep_names: Vec<&str> = ctx
        .iter()
        .filter(|e| e["role"] == "direct_dependency")
        .map(|e| e["symbol"].as_str().unwrap())
        .collect();
    assert!(dep_names.contains(&"fn_b"), "fn_b should be a dep");
    assert!(dep_names.contains(&"fn_c"), "fn_c should be a dep");

    // Should have direct dependents (fn_d, but NOT test_fn_a — tests are tallied)
    let dept_names: Vec<&str> = ctx
        .iter()
        .filter(|e| e["role"] == "direct_dependent")
        .map(|e| e["symbol"].as_str().unwrap())
        .collect();
    assert!(dept_names.contains(&"fn_d"), "fn_d should be a dependent");
    assert!(
        !dept_names.contains(&"test_fn_a"),
        "test should not be packed"
    );

    // Test should be in omitted
    if let Some(omitted) = result["omitted"].as_array() {
        let test_omitted = omitted
            .iter()
            .find(|o| o["role"] == "direct_dependent")
            .expect("should have direct_dependent omitted entry");
        assert!(test_omitted["tests"].as_u64().unwrap() >= 1);
    }

    // Token accounting
    let tokens_used = result["tokens_used"].as_u64().unwrap();
    let budget = result["token_budget"].as_u64().unwrap();
    assert!(tokens_used > 0);
    assert!(tokens_used <= budget);
}

#[test]
fn tight_budget_truncates_target() {
    let (dir, db) = setup_workspace();
    // Very tight budget — target should be head-truncated or signature
    let result = context::handle(&db, dir.path(), "fn_a", Some(10), Some(1), false, None).unwrap();

    assert_eq!(result["symbol"], "fn_a");
    assert_eq!(result["truncated"], true);
    assert_eq!(result["target_omitted"], false);

    let ctx = result["context"].as_array().unwrap();
    assert!(!ctx.is_empty());
    let target = &ctx[0];
    assert_eq!(target["role"], "target");
    let tokens = target["tokens"].as_u64().unwrap();
    assert!(tokens <= 10);
}

#[test]
fn budget_too_small_omits_target() {
    let (dir, db) = setup_workspace();
    let result = context::handle(&db, dir.path(), "fn_a", Some(1), Some(1), false, None).unwrap();

    assert_eq!(result["target_omitted"], true);
    assert_eq!(result["truncated"], true);
    let ctx = result["context"].as_array().unwrap();
    assert!(ctx.is_empty());
}

#[test]
fn unknown_symbol_returns_diagnostic() {
    let (dir, db) = setup_workspace();
    let result = context::handle(&db, dir.path(), "nonexistent", None, None, false, None).unwrap();

    assert!(result.get("diagnostic").is_some());
}

#[test]
fn stale_index_refused() {
    let (dir, db) = setup_workspace();
    let result = context::handle(&db, dir.path(), "fn_a", None, None, true, None).unwrap();

    assert!(result["refused"].as_str().is_some());
    assert!(result["refused"].as_str().unwrap().contains("stale"));
}

#[test]
fn neighbor_cap_respected() {
    let (dir, db) = setup_workspace();
    // With a moderate budget, neighbors should not exceed target cost
    let result = context::handle(&db, dir.path(), "fn_a", Some(50), Some(1), false, None).unwrap();

    let ctx = result["context"].as_array().unwrap();
    if ctx.len() > 1 {
        let target_tokens = ctx[0]["tokens"].as_u64().unwrap();
        let budget_floor = 50u64 / 10;
        let cap = target_tokens.max(budget_floor);
        for entry in &ctx[1..] {
            let entry_tokens = entry["tokens"].as_u64().unwrap();
            assert!(
                entry_tokens <= cap,
                "neighbor {} ({} tokens) exceeded cap ({})",
                entry["symbol"],
                entry_tokens,
                cap
            );
        }
    }
}

#[test]
fn estimate_tokens_basic() {
    assert!(context::estimate_tokens("fn foo() -> i32") > 0);
    assert_eq!(context::estimate_tokens(""), 1);

    let long = "x".repeat(400);
    let by_chars = 400 / 4;
    assert_eq!(context::estimate_tokens(&long), by_chars);
}
