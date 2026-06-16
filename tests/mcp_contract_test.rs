use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use sutra::db::{Db, InsertSymbolParams, SnapshotParams};
use sutra::tools::{
    ToolContext, deps, explore, find, grep, impact, map, outline, read, refs, tools_meta, winnow,
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
    let result = read::handle(
        &db,
        dir.path(),
        "main",
        None,
        None,
        false,
        false,
        true,
        None,
    )
    .unwrap();
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
    let result =
        read::handle(&db, dir.path(), "main", None, None, false, true, true, None).unwrap();
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

fn setup_explore_db() -> (tempfile::TempDir, Db) {
    setup_explore_db_inner(false)
}

fn setup_explore_db_with_calls() -> (tempfile::TempDir, Db) {
    setup_explore_db_inner(true)
}

fn setup_explore_db_inner(with_calls: bool) -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("explore_test", dir.path()).unwrap();

    db.upsert_file("src/parser.rs", "rust", "hash1", 100, true)
        .unwrap();
    let file = db.file_by_path("src/parser.rs").unwrap().unwrap();

    let syms: Vec<(&str, &str, i64, i64)> = vec![
        ("parse_imports", "parse_imports", 1, 20),
        ("resolve_imports", "resolve_imports", 25, 50),
        ("handle_exports", "handle_exports", 55, 70),
        ("build_ast", "build_ast", 75, 100),
    ];

    let mut sym_ids = Vec::new();
    for (qn, sn, start, end) in &syms {
        let id = db
            .insert_symbol(&InsertSymbolParams {
                file_id: file.id,
                qualified_name: qn,
                short_name: sn,
                kind: "function",
                signature: Some(&format!("fn {sn}()")),
                signature_hash: None,
                visibility: Some("pub"),
                start_line: *start,
                start_col: 0,
                end_line: *end,
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
        sym_ids.push(id);
    }

    if with_calls {
        // parse_imports calls resolve_imports (ref at line 10, within parse_imports body 1-20)
        db.insert_ref(file.id, Some(sym_ids[1]), None, 10, 4, "call")
            .unwrap();
        // build_ast calls parse_imports (ref at line 80, within build_ast body 75-100)
        db.insert_ref(file.id, Some(sym_ids[0]), None, 80, 4, "call")
            .unwrap();
    }

    db.insert_snapshot(&SnapshotParams {
        files_parsed: 1,
        symbols_extracted: 4,
        refs_extracted: if with_calls { 2 } else { 0 },
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
fn test_explore_basic() {
    let (_dir, db) = setup_explore_db();
    let result = explore::handle(&db, "import", 10).unwrap();

    let items = result["items"].as_array().expect("items array");
    assert!(
        items.len() >= 2,
        "should match parse_imports and resolve_imports, got {}",
        items.len()
    );
    for item in items {
        assert!(item["symbol"].is_string());
        assert!(item["file"].is_string());
        assert!(item["kind"].is_string());
        assert!(item["lines"].is_i64());
        assert!(item["estimated_tokens"].is_i64());
        let fetch = item["fetch"].as_str().unwrap();
        assert!(
            fetch.starts_with("sutra_read(symbol='"),
            "fetch must be a literal sutra_read call, got: {fetch}"
        );
    }
    assert!(result["strategy"]["action"].is_string());
    assert!(result["strategy"]["rationale"].is_string());
    let summary = &result["summary"];
    assert!(summary["total_items"].is_i64());
    assert!(summary["direct_matches"].is_i64());
    assert!(summary["fan_out_items"].is_i64());
    assert!(summary["components_touched"].is_i64());
    assert!(summary["total_estimated_tokens"].is_i64());
    assert_eq!(summary["fan_out_items"].as_i64().unwrap(), 0);
}

#[test]
fn test_explore_zero_hits() {
    let (_dir, db) = setup_explore_db();
    let result = explore::handle(&db, "nonexistent_xyzzy", 10).unwrap();

    let items = result["items"].as_array().unwrap();
    assert!(items.is_empty());
    assert_eq!(
        result["strategy"]["action"].as_str().unwrap(),
        "narrow_query"
    );
}

#[test]
fn test_explore_budget_limits_items() {
    let (_dir, db) = setup_explore_db();
    let result = explore::handle(&db, "parse_imports", 1).unwrap();

    let items = result["items"].as_array().unwrap();
    assert!(items.len() <= 1, "budget=1 should return at most 1 item");
}

#[test]
fn test_explore_negative_budget_clamps() {
    let (_dir, db) = setup_explore_db();
    let result = explore::handle(&db, "parse_imports", -5).unwrap();

    let items = result["items"].as_array().unwrap();
    assert!(items.len() <= 1, "negative budget should clamp to 1");
}

#[test]
fn test_explore_reason_field() {
    let (_dir, db) = setup_explore_db();
    let result = explore::handle(&db, "import", 10).unwrap();

    let items = result["items"].as_array().unwrap();
    for item in items {
        assert_eq!(
            item["reason"].as_str().unwrap(),
            "direct_match",
            "without call edges, all items should be direct_match"
        );
    }
}

#[test]
fn test_explore_fan_out_few_hits() {
    // "build_ast" matches 1 symbol → 1-3 range → 2-hop fan-out
    // build_ast calls parse_imports, parse_imports calls resolve_imports
    // So fan-out should surface parse_imports (hop 1) and resolve_imports (hop 2)
    let (_dir, db) = setup_explore_db_with_calls();
    let result = explore::handle(&db, "build_ast", 10).unwrap();

    let items = result["items"].as_array().unwrap();
    let direct: Vec<_> = items
        .iter()
        .filter(|i| i["reason"].as_str() == Some("direct_match"))
        .collect();
    let fan_out: Vec<_> = items
        .iter()
        .filter(|i| i["reason"].as_str() == Some("fan_out"))
        .collect();

    assert_eq!(direct.len(), 1, "build_ast is the only direct match");
    assert!(
        fan_out.len() >= 1,
        "should have at least 1 fan-out item, got {}",
        fan_out.len()
    );

    let fan_out_names: Vec<&str> = fan_out
        .iter()
        .filter_map(|i| i["symbol"].as_str())
        .collect();
    assert!(
        fan_out_names.contains(&"parse_imports"),
        "parse_imports should be a fan-out item (callee of build_ast), got: {fan_out_names:?}"
    );

    let summary = &result["summary"];
    assert!(
        summary["fan_out_items"].as_i64().unwrap() >= 1,
        "summary should report fan-out items"
    );
    assert_eq!(
        summary["direct_matches"].as_i64().unwrap(),
        1,
        "summary should report 1 direct match"
    );
}

#[test]
fn test_explore_fan_out_score_decay() {
    // Fan-out items should rank below direct matches
    let (_dir, db) = setup_explore_db_with_calls();
    let result = explore::handle(&db, "build_ast", 10).unwrap();

    let items = result["items"].as_array().unwrap();
    // First item should be the direct match
    assert_eq!(items[0]["reason"].as_str().unwrap(), "direct_match");
    // All subsequent should be fan_out
    for item in &items[1..] {
        assert_eq!(item["reason"].as_str().unwrap(), "fan_out");
    }
}

#[test]
fn test_explore_edges() {
    let (_dir, db) = setup_explore_db_with_calls();
    let result = explore::handle(&db, "build_ast", 10).unwrap();

    let edges = result["edges"]
        .as_array()
        .expect("edges array should exist");
    // build_ast calls parse_imports — both should be in the response
    let has_build_to_parse = edges.iter().any(|e| {
        e["from"].as_str() == Some("build_ast")
            && e["to"].as_str() == Some("parse_imports")
            && e["kind"].as_str() == Some("call")
    });
    assert!(
        has_build_to_parse,
        "should have edge from build_ast to parse_imports, edges: {edges:?}"
    );

    // All edge endpoints should be in the response items
    let item_names: Vec<&str> = result["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["symbol"].as_str())
        .collect();
    for edge in edges {
        let from = edge["from"].as_str().unwrap();
        let to = edge["to"].as_str().unwrap();
        assert!(
            item_names.contains(&from),
            "edge 'from' {from} not in response items"
        );
        assert!(
            item_names.contains(&to),
            "edge 'to' {to} not in response items"
        );
    }
}
