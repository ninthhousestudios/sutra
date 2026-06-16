use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use sutra::db::{Db, InsertSymbolParams, SnapshotParams};
use sutra::tools::{
    ToolContext, deps, find, grep, impact, map, outline, read, refs, tools_meta, winnow,
};

fn setup_test_db_with_root() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("contract_test", dir.path()).unwrap();

    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("main.rs"), "fn main() {}").unwrap();

    db.upsert_file("src/main.rs", "rust", "hash1", 50, true)
        .unwrap();
    let file = db.file_by_path("src/main.rs").unwrap().unwrap();
    db.insert_symbol(&InsertSymbolParams {
        file_id: file.id,
        qualified_name: "main",
        short_name: "main",
        kind: "function",
        signature: Some("fn main()"),
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
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 1,
        symbols_extracted: 1,
        refs_extracted: 0,
        parse_errors: 0,
        duration_ms: 100,
        total_complexity: 0,
        dead_symbol_count: 0,
        hotspot_count: 0,
        health_score: 0.0,
        ..Default::default()
    })
    .unwrap();

    (dir, db)
}

fn setup_test_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("contract_test", dir.path()).unwrap();

    db.upsert_file("src/main.rs", "rust", "hash1", 50, true)
        .unwrap();
    let file = db.file_by_path("src/main.rs").unwrap().unwrap();
    db.insert_symbol(&InsertSymbolParams {
        file_id: file.id,
        qualified_name: "main",
        short_name: "main",
        kind: "function",
        signature: Some("fn main()"),
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
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 1,
        symbols_extracted: 1,
        refs_extracted: 0,
        parse_errors: 0,
        duration_ms: 100,
        total_complexity: 0,
        dead_symbol_count: 0,
        hotspot_count: 0,
        health_score: 0.0,
        ..Default::default()
    })
    .unwrap();

    (dir, db)
}

#[test]
fn test_map_contract() {
    let (_dir, db) = setup_test_db();
    let result = map::handle(&db, None, None, false).unwrap();

    let files = result["files"]
        .as_array()
        .expect("map result must have 'files' array");
    assert!(!files.is_empty());

    let entry = &files[0];
    assert!(entry["path"].is_string(), "'path' must be a string");
    assert!(entry["language"].is_string(), "'language' must be a string");
    assert!(entry["symbols"].is_number(), "'symbols' must be a number");
    assert!(
        entry["line_count"].is_number(),
        "'line_count' must be a number"
    );
}

#[test]
fn test_outline_contract() {
    let (_dir, db) = setup_test_db();
    let result = outline::handle(&db, "src/main.rs", false).unwrap();

    assert!(result["path"].is_string(), "'path' must be a string");
    let symbols = result["symbols"]
        .as_array()
        .expect("outline result must have 'symbols' array");
    assert!(!symbols.is_empty());

    let sym = &symbols[0];
    assert!(
        sym["qualified_name"].is_string(),
        "'qualified_name' must be a string"
    );
    assert!(sym["kind"].is_string(), "'kind' must be a string");
    assert!(
        sym["start_line"].is_number(),
        "'start_line' must be a number"
    );
}

#[test]
fn test_find_contract() {
    let (_dir, db) = setup_test_db();
    let result = find::handle(&db, "main", None, None, false).unwrap();

    let matches = result["matches"]
        .as_array()
        .expect("find result must have 'matches' array");
    assert!(!matches.is_empty());

    let hit = &matches[0];
    assert!(hit["qualified_name"].is_string());
    assert!(hit["kind"].is_string());
}

#[test]
fn test_grep_contract() {
    let (_dir, db) = setup_test_db();
    let result = grep::handle(&db, "main", None, None, false).unwrap();

    let matches = result["matches"]
        .as_array()
        .expect("grep result must have 'matches' array");
    assert!(!matches.is_empty());

    let hit = &matches[0];
    assert!(hit["qualified_name"].is_string());
    assert!(hit["kind"].is_string());
}

#[test]
fn test_impact_contract() {
    let (_dir, db) = setup_test_db();
    let result = impact::handle(&db, "main", false).unwrap();

    assert!(result["symbol"].is_string(), "'symbol' must be a string");
    assert!(result["risk"].is_string(), "'risk' must be a string");
    assert!(
        result["direct_callers"].is_number(),
        "'direct_callers' must be a number"
    );
    assert!(
        result["files_touched"].is_number(),
        "'files_touched' must be a number"
    );
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
    assert!(
        result["total_edges"].is_number(),
        "'total_edges' must be a number"
    );
}

#[test]
fn test_tools_meta_contract() {
    let flag = AtomicBool::new(false);
    let result = tools_meta::handle(&flag, None, None, true);

    let tiers = result["tiers"]
        .as_object()
        .expect("tools_meta must have 'tiers' object");
    assert!(tiers.contains_key("core"), "tiers must include 'core'");
    assert!(
        tiers.contains_key("analysis"),
        "tiers must include 'analysis'"
    );

    let core = &tiers["core"];
    assert_eq!(core["enabled"], true, "core tier must always be enabled");
    let tools = core["tools"].as_array().expect("core must list tools");
    assert!(!tools.is_empty());
}

#[test]
fn test_find_not_found() {
    let (_dir, db) = setup_test_db();
    let result = find::handle(&db, "nonexistent_symbol_xyz", None, None, false).unwrap();

    let matches = result["matches"].as_array().unwrap();
    assert!(
        matches.is_empty(),
        "nonexistent symbol should return empty matches, not an error"
    );
}

#[test]
fn test_outline_not_found() {
    let (_dir, db) = setup_test_db();
    let result = outline::handle(&db, "src/does_not_exist.rs", true);

    assert!(
        result.is_err(),
        "outline of nonexistent file must return an error"
    );
}

// ---------------------------------------------------------------------------
// Freshness + confidence contract tests
// ---------------------------------------------------------------------------

fn freshness_ctx(dir: &tempfile::TempDir, db: Db) -> ToolContext {
    ToolContext::for_test_with_freshness(Arc::new(db), dir.path().to_path_buf())
}

#[test]
fn test_map_freshness_per_entry() {
    let (dir, db) = setup_test_db_with_root();
    let ctx = freshness_ctx(&dir, db);
    let result = map::handle_ctx(&ctx, None, None, false).unwrap();

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
    assert!(
        meta["freshness"]["fresh"].is_number(),
        "_meta.freshness.fresh must exist"
    );
    assert!(
        meta["freshness"]["edited"].is_number(),
        "_meta.freshness.edited must exist"
    );
    assert!(
        meta["freshness"]["stale"].is_number(),
        "_meta.freshness.stale must exist"
    );
}

#[test]
fn test_find_freshness_and_confidence() {
    let (dir, db) = setup_test_db_with_root();
    let ctx = freshness_ctx(&dir, db);
    let result = find::handle_ctx(&ctx, "main", None, None, false).unwrap();

    let matches = result["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    for entry in matches {
        assert!(
            entry["_freshness"].is_string(),
            "find entry must have _freshness"
        );
    }

    let meta = &result["_meta"];
    assert!(meta["freshness"]["fresh"].is_number());
    assert!(
        meta["confidence"]["score"].is_number(),
        "_meta.confidence.score must exist"
    );
    assert!(
        meta["confidence"]["tier"].is_string(),
        "_meta.confidence.tier must exist"
    );
    assert!(
        meta["confidence"]["formula"].is_string(),
        "_meta.confidence.formula must exist"
    );

    let score = meta["confidence"]["score"].as_f64().unwrap();
    assert!(
        score > 0.0 && score <= 1.0,
        "confidence score must be in (0, 1], got: {score}"
    );
}

#[test]
fn test_grep_freshness_and_confidence() {
    let (dir, db) = setup_test_db_with_root();
    let ctx = freshness_ctx(&dir, db);
    let result = grep::handle_ctx(&ctx, "main", None, None, false).unwrap();

    let matches = result["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    for entry in matches {
        assert!(
            entry["_freshness"].is_string(),
            "grep entry must have _freshness"
        );
    }

    let meta = &result["_meta"];
    assert!(meta["confidence"]["score"].is_number());
    assert!(meta["confidence"]["tier"].is_string());
}

#[test]
fn test_find_exact_match_confidence_is_1() {
    let (dir, db) = setup_test_db_with_root();
    let ctx = freshness_ctx(&dir, db);
    let result = find::handle_ctx(&ctx, "main", None, None, false).unwrap();
    let score = result["_meta"]["confidence"]["score"].as_f64().unwrap();
    assert_eq!(
        score, 1.0,
        "exact short_name match should yield confidence 1.0"
    );
    assert_eq!(
        result["_meta"]["confidence"]["tier"].as_str().unwrap(),
        "exact"
    );
}

#[test]
fn test_find_fts_match_confidence_below_1() {
    let (dir, db) = setup_test_db_with_root();
    let ctx = freshness_ctx(&dir, db);
    let result = find::handle_ctx(&ctx, "mai", None, None, false).unwrap();
    let matches = result["matches"].as_array().unwrap();
    if !matches.is_empty() {
        let score = result["_meta"]["confidence"]["score"].as_f64().unwrap();
        assert!(score < 1.0, "FTS match should yield confidence < 1.0");
        assert_eq!(
            result["_meta"]["confidence"]["tier"].as_str().unwrap(),
            "fts"
        );
    }
}

#[test]
fn test_map_without_freshness_has_no_meta() {
    let (_dir, db) = setup_test_db();
    let result = map::handle(&db, None, None, false).unwrap();
    assert!(
        result.get("_meta").is_none(),
        "plain handle() should not include _meta"
    );
}

// ---------------------------------------------------------------------------
// read contract tests — tier-2 freshness enforcement
// ---------------------------------------------------------------------------

#[test]
fn test_read_fresh_returns_content() {
    let (dir, db) = setup_test_db_with_root();
    let result = read::handle(&db, dir.path(), "main", None, None, false, false, None).unwrap();
    assert!(
        result["content"].is_string(),
        "fresh read should include content"
    );
    assert!(
        result.get("refused").is_none(),
        "fresh read should not have refused field"
    );
}

#[test]
fn test_read_stale_withholds_content() {
    let (dir, db) = setup_test_db_with_root();
    let result = read::handle(&db, dir.path(), "main", None, None, false, true, None).unwrap();
    assert!(
        result.get("content").is_none(),
        "stale read must not include content"
    );
    assert_eq!(
        result["refused"].as_str().unwrap(),
        "content withheld: index is stale"
    );
    assert!(
        result["next_action"].is_string(),
        "stale read should suggest next action"
    );
    assert!(
        result["symbol"].is_string(),
        "stale read should still include symbol metadata"
    );
    assert!(
        result["signature"].is_string(),
        "stale read should still include signature"
    );
}

// ---------------------------------------------------------------------------
// winnow contract tests
// ---------------------------------------------------------------------------

fn winnow_filter_default() -> winnow::WinnowFilter {
    winnow::WinnowFilter {
        kind: None,
        min_complexity: None,
        min_churn: None,
        churn_window_days: None,
        calls_to: None,
        file_glob: None,
        name_regex: None,
        rank_by: None,
        limit: None,
    }
}

#[test]
fn test_winnow_no_filters() {
    let (dir, db) = setup_test_db_with_root();
    let filter = winnow_filter_default();
    let result = winnow::handle(&db, dir.path(), &filter).unwrap();

    let matches = result["matches"].as_array().unwrap();
    assert!(
        !matches.is_empty(),
        "winnow with no filters should return all symbols"
    );
    let entry = &matches[0];
    assert!(entry["qualified_name"].is_string());
    assert!(entry["kind"].is_string());
    assert!(entry["file"].is_string());
    assert!(
        entry["axes"]["importance"].is_number(),
        "each entry must include axes.importance"
    );
    assert!(
        entry["axes"]["complexity"].is_number(),
        "each entry must include axes.complexity"
    );
    assert!(
        entry["axes"]["churn"].is_number(),
        "each entry must include axes.churn"
    );
    assert!(
        entry["_freshness"].is_string(),
        "each entry must include _freshness"
    );

    assert!(result["_meta"]["freshness"]["fresh"].is_number());
}

#[test]
fn test_winnow_kind_filter() {
    let (dir, db) = setup_test_db_with_root();
    let mut filter = winnow_filter_default();
    filter.kind = Some("function".to_string());
    let result = winnow::handle(&db, dir.path(), &filter).unwrap();
    let matches = result["matches"].as_array().unwrap();
    for m in matches {
        assert_eq!(m["kind"].as_str().unwrap(), "function");
    }
}

#[test]
fn test_winnow_kind_filter_no_match() {
    let (dir, db) = setup_test_db_with_root();
    let mut filter = winnow_filter_default();
    filter.kind = Some("trait".to_string());
    let result = winnow::handle(&db, dir.path(), &filter).unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(
        matches.is_empty(),
        "filtering by nonexistent kind should return empty"
    );
}

#[test]
fn test_winnow_name_regex() {
    let (dir, db) = setup_test_db_with_root();
    let mut filter = winnow_filter_default();
    filter.name_regex = Some("^main$".to_string());
    let result = winnow::handle(&db, dir.path(), &filter).unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    for m in matches {
        let name = m["short_name"].as_str().unwrap();
        assert_eq!(name, "main");
    }
}

#[test]
fn test_winnow_file_glob() {
    let (dir, db) = setup_test_db_with_root();
    let mut filter = winnow_filter_default();
    filter.file_glob = Some("src/*.rs".to_string());
    let result = winnow::handle(&db, dir.path(), &filter).unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(!matches.is_empty());
    for m in matches {
        let file = m["file"].as_str().unwrap();
        assert!(file.starts_with("src/"), "file glob should filter to src/");
    }
}

#[test]
fn test_winnow_file_glob_no_match() {
    let (dir, db) = setup_test_db_with_root();
    let mut filter = winnow_filter_default();
    filter.file_glob = Some("nonexistent/*.rs".to_string());
    let result = winnow::handle(&db, dir.path(), &filter).unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(matches.is_empty());
}

#[test]
fn test_winnow_min_complexity_filters() {
    let (dir, db) = setup_test_db_with_root();
    let mut filter = winnow_filter_default();
    filter.min_complexity = Some(9999);
    let result = winnow::handle(&db, dir.path(), &filter).unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(
        matches.is_empty(),
        "no symbol should have complexity >= 9999"
    );
}

#[test]
fn test_winnow_rank_by_complexity() {
    let (dir, db) = setup_test_db_with_root();

    // Add a second symbol with higher complexity
    db.insert_symbol(&InsertSymbolParams {
        file_id: db.file_by_path("src/main.rs").unwrap().unwrap().id,
        qualified_name: "complex_fn",
        short_name: "complex_fn",
        kind: "function",
        signature: Some("fn complex_fn()"),
        signature_hash: None,
        visibility: Some("pub"),
        start_line: 20,
        start_col: 0,
        end_line: 50,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: Some(15),
        cognitive: Some(25),
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    })
    .unwrap();

    let mut filter = winnow_filter_default();
    filter.rank_by = Some("complexity".to_string());
    let result = winnow::handle(&db, dir.path(), &filter).unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(matches.len() >= 2);
    let c0 = matches[0]["axes"]["complexity"].as_i64().unwrap();
    let c1 = matches[1]["axes"]["complexity"].as_i64().unwrap();
    assert!(
        c0 >= c1,
        "results should be ranked by complexity descending"
    );
}

#[test]
fn test_refs_context_kind_filter() {
    let (_dir, db) = setup_test_db();
    let file = db.file_by_path("src/main.rs").unwrap().unwrap();

    db.insert_symbol(&InsertSymbolParams {
        file_id: file.id,
        qualified_name: "Config",
        short_name: "Config",
        kind: "struct",
        signature: None,
        signature_hash: None,
        visibility: Some("pub"),
        start_line: 20,
        start_col: 0,
        end_line: 25,
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

    let config_sym = db.resolve_symbol("Config", None).unwrap().unwrap();

    db.insert_ref(file.id, Some(config_sym.id), None, 30, 4, "construction")
        .unwrap();
    db.insert_ref(file.id, Some(config_sym.id), None, 40, 4, "type_use")
        .unwrap();
    db.insert_ref(file.id, Some(config_sym.id), None, 50, 4, "call")
        .unwrap();

    let all = refs::handle(&db, "Config", None).unwrap();
    assert_eq!(all["total_refs"].as_i64().unwrap(), 3);

    let filtered = refs::handle(&db, "Config", Some("construction")).unwrap();
    assert_eq!(
        filtered["total_refs"].as_i64().unwrap(),
        1,
        "filter should return only construction refs"
    );
    let locs = &filtered["references"][0]["locations"];
    assert_eq!(locs[0]["context_kind"], "construction");

    let no_match = refs::handle(&db, "Config", Some("pattern_bind")).unwrap();
    assert_eq!(
        no_match["total_refs"].as_i64().unwrap(),
        0,
        "filter with no matches should return 0 refs"
    );
}
