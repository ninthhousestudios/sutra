use std::sync::atomic::AtomicBool;

use sutra::db::{Db, InsertSymbolParams};
use sutra::tools::{deps, find, grep, impact, map, outline, tools_meta};

fn setup_test_db_with_root() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("contract_test", dir.path()).unwrap();

    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() {}").unwrap();

    db.upsert_file("src/main.rs", "rust", "hash1", 50, true).unwrap();
    let file = db.file_by_path("src/main.rs").unwrap().unwrap();
    db.insert_symbol(&InsertSymbolParams {
        file_id: file.id, qualified_name: "main", short_name: "main", kind: "function",
        signature: Some("fn main()"), signature_hash: None, visibility: Some("pub"),
        start_line: 1, start_col: 0, end_line: 10, end_col: 0,
        parent_symbol_id: None, docstring: None,
        cyclomatic: None, cognitive: None,
    }).unwrap();
    db.insert_snapshot(1, 1, 0, 0, 100).unwrap();

    (dir, db)
}

fn setup_test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("contract_test", dir.path()).unwrap();

    db.upsert_file("src/main.rs", "rust", "hash1", 50, true).unwrap();
    let file = db.file_by_path("src/main.rs").unwrap().unwrap();
    db.insert_symbol(&InsertSymbolParams {
        file_id: file.id, qualified_name: "main", short_name: "main", kind: "function",
        signature: Some("fn main()"), signature_hash: None, visibility: Some("pub"),
        start_line: 1, start_col: 0, end_line: 10, end_col: 0,
        parent_symbol_id: None, docstring: None,
            cyclomatic: None,
            cognitive: None,
    }).unwrap();
    db.insert_snapshot(1, 1, 0, 0, 100).unwrap();

    (dir, db)
}

#[test]
fn test_map_contract() {
    let (_dir, db) = setup_test_db();
    let result = map::handle(&db, None, None).unwrap();

    let files = result["files"].as_array().expect("map result must have 'files' array");
    assert!(!files.is_empty());

    let entry = &files[0];
    assert!(entry["path"].is_string(), "'path' must be a string");
    assert!(entry["language"].is_string(), "'language' must be a string");
    assert!(entry["symbols"].is_number(), "'symbols' must be a number");
    assert!(entry["line_count"].is_number(), "'line_count' must be a number");
}

#[test]
fn test_outline_contract() {
    let (_dir, db) = setup_test_db();
    let result = outline::handle(&db, "src/main.rs").unwrap();

    assert!(result["path"].is_string(), "'path' must be a string");
    let symbols = result["symbols"].as_array().expect("outline result must have 'symbols' array");
    assert!(!symbols.is_empty());

    let sym = &symbols[0];
    assert!(sym["qualified_name"].is_string(), "'qualified_name' must be a string");
    assert!(sym["kind"].is_string(), "'kind' must be a string");
    assert!(sym["start_line"].is_number(), "'start_line' must be a number");
}

#[test]
fn test_find_contract() {
    let (_dir, db) = setup_test_db();
    let result = find::handle(&db, "main", None, None).unwrap();

    let matches = result["matches"].as_array().expect("find result must have 'matches' array");
    assert!(!matches.is_empty());

    let hit = &matches[0];
    assert!(hit["qualified_name"].is_string());
    assert!(hit["kind"].is_string());
}

#[test]
fn test_grep_contract() {
    let (_dir, db) = setup_test_db();
    let result = grep::handle(&db, "main", None, None).unwrap();

    let matches = result["matches"].as_array().expect("grep result must have 'matches' array");
    assert!(!matches.is_empty());

    let hit = &matches[0];
    assert!(hit["qualified_name"].is_string());
    assert!(hit["kind"].is_string());
}

#[test]
fn test_impact_contract() {
    let (_dir, db) = setup_test_db();
    let result = impact::handle(&db, "main").unwrap();

    assert!(result["symbol"].is_string(), "'symbol' must be a string");
    assert!(result["risk"].is_string(), "'risk' must be a string");
    assert!(result["direct_callers"].is_number(), "'direct_callers' must be a number");
    assert!(result["files_touched"].is_number(), "'files_touched' must be a number");
}

#[test]
fn test_deps_contract() {
    let (_dir, db) = setup_test_db();
    let result = deps::handle(&db, Some("src/main.rs"), None).unwrap();

    assert!(result["nodes"].is_array(), "'nodes' must be an array");
    assert!(result["edges"].is_array(), "'edges' must be an array");
}

#[test]
fn test_deps_global_contract() {
    let (_dir, db) = setup_test_db();
    let result = deps::handle(&db, None, None).unwrap();

    assert!(result["edges"].is_array(), "'edges' must be an array");
    assert!(result["total_edges"].is_number(), "'total_edges' must be a number");
}

#[test]
fn test_tools_meta_contract() {
    let flag = AtomicBool::new(false);
    let result = tools_meta::handle(&flag, None, None, true);

    let tiers = result["tiers"].as_object().expect("tools_meta must have 'tiers' object");
    assert!(tiers.contains_key("core"), "tiers must include 'core'");
    assert!(tiers.contains_key("analysis"), "tiers must include 'analysis'");

    let core = &tiers["core"];
    assert_eq!(core["enabled"], true, "core tier must always be enabled");
    let tools = core["tools"].as_array().expect("core must list tools");
    assert!(!tools.is_empty());
}

#[test]
fn test_find_not_found() {
    let (_dir, db) = setup_test_db();
    let result = find::handle(&db, "nonexistent_symbol_xyz", None, None).unwrap();

    let matches = result["matches"].as_array().unwrap();
    assert!(matches.is_empty(), "nonexistent symbol should return empty matches, not an error");
}

#[test]
fn test_outline_not_found() {
    let (_dir, db) = setup_test_db();
    let result = outline::handle(&db, "src/does_not_exist.rs");

    assert!(result.is_err(), "outline of nonexistent file must return an error");
}

// ---------------------------------------------------------------------------
// Freshness + confidence contract tests
// ---------------------------------------------------------------------------

#[test]
fn test_map_freshness_per_entry() {
    let (dir, db) = setup_test_db_with_root();
    let result = map::handle_with_freshness(&db, None, None, Some(dir.path())).unwrap();

    let files = result["files"].as_array().unwrap();
    assert!(!files.is_empty());
    for entry in files {
        assert!(
            entry["_freshness"].is_string(),
            "each map entry must have _freshness"
        );
        let f = entry["_freshness"].as_str().unwrap();
        assert!(
            f == "fresh" || f == "edited" || f == "stale",
            "_freshness must be fresh/edited/stale, got: {f}"
        );
    }

    let meta = &result["_meta"];
    assert!(meta["freshness"]["fresh"].is_number(), "_meta.freshness.fresh must exist");
    assert!(meta["freshness"]["edited"].is_number(), "_meta.freshness.edited must exist");
    assert!(meta["freshness"]["stale"].is_number(), "_meta.freshness.stale must exist");
}

#[test]
fn test_find_freshness_and_confidence() {
    let (dir, db) = setup_test_db_with_root();
    let result = find::handle_with_freshness(&db, "main", None, None, Some(dir.path())).unwrap();

    let matches = result["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    for entry in matches {
        assert!(entry["_freshness"].is_string(), "find entry must have _freshness");
    }

    let meta = &result["_meta"];
    assert!(meta["freshness"]["fresh"].is_number());
    assert!(meta["confidence"]["score"].is_number(), "_meta.confidence.score must exist");
    assert!(meta["confidence"]["tier"].is_string(), "_meta.confidence.tier must exist");
    assert!(meta["confidence"]["formula"].is_string(), "_meta.confidence.formula must exist");

    let score = meta["confidence"]["score"].as_f64().unwrap();
    assert!(score > 0.0 && score <= 1.0, "confidence score must be in (0, 1], got: {score}");
}

#[test]
fn test_grep_freshness_and_confidence() {
    let (dir, db) = setup_test_db_with_root();
    let result = grep::handle_with_freshness(&db, "main", None, None, Some(dir.path())).unwrap();

    let matches = result["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    for entry in matches {
        assert!(entry["_freshness"].is_string(), "grep entry must have _freshness");
    }

    let meta = &result["_meta"];
    assert!(meta["confidence"]["score"].is_number());
    assert!(meta["confidence"]["tier"].is_string());
}

#[test]
fn test_find_exact_match_confidence_is_1() {
    let (dir, db) = setup_test_db_with_root();
    let result = find::handle_with_freshness(&db, "main", None, None, Some(dir.path())).unwrap();
    let score = result["_meta"]["confidence"]["score"].as_f64().unwrap();
    assert_eq!(score, 1.0, "exact short_name match should yield confidence 1.0");
    assert_eq!(result["_meta"]["confidence"]["tier"].as_str().unwrap(), "exact");
}

#[test]
fn test_find_fts_match_confidence_below_1() {
    let (dir, db) = setup_test_db_with_root();
    let result = find::handle_with_freshness(&db, "mai", None, None, Some(dir.path())).unwrap();
    let matches = result["matches"].as_array().unwrap();
    if !matches.is_empty() {
        let score = result["_meta"]["confidence"]["score"].as_f64().unwrap();
        assert!(score < 1.0, "FTS match should yield confidence < 1.0");
        assert_eq!(result["_meta"]["confidence"]["tier"].as_str().unwrap(), "fts");
    }
}

#[test]
fn test_map_without_freshness_has_no_meta() {
    let (_dir, db) = setup_test_db();
    let result = map::handle(&db, None, None).unwrap();
    assert!(result.get("_meta").is_none(), "plain handle() should not include _meta");
}
