use sutra::db::{Db, InsertSymbolParams};
use sutra::health::findings::{BiomarkerKind, HealthFinding, HealthSeverity};
use sutra::tools::review::ReviewFindings;
use sutra::tools::{file_health, impact, map, pr_risk, review};

fn sym<'a>(
    file_id: i64,
    qn: &'a str,
    sn: &'a str,
    cognitive: Option<i64>,
) -> InsertSymbolParams<'a> {
    InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind: "function",
        signature: Some("fn()"),
        signature_hash: None,
        structural_hash: None,
        visibility: Some("pub"),
        start_line: 1,
        start_col: 0,
        end_line: 10,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive,
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    }
}

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("explain_test", dir.path()).unwrap();

    db.upsert_file("src/a.rs", "rust", "ha", 100, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 50, true).unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();

    db.insert_symbol(&sym(fa.id, "a::target_fn", "target_fn", Some(5)))
        .unwrap();
    db.insert_symbol(&sym(fb.id, "b::caller_fn", "caller_fn", Some(12)))
        .unwrap();

    db.update_rollups(fa.id, 2, 10).unwrap();
    db.update_rollups(fb.id, 1, 5).unwrap();

    // caller_fn calls target_fn: ref is in b.rs (the caller's file) targeting target_fn
    let target = db.resolve_symbol("a::target_fn", None).unwrap().unwrap();
    db.insert_ref(fb.id, Some(target.id), None, 5, 0, "call")
        .unwrap();

    (dir, db)
}

#[test]
fn map_explain_false_has_no_explain_key() {
    let (_dir, db) = setup_db();
    let result = map::handle(&db, None, None, false).unwrap();
    let files = result["files"].as_array().unwrap();
    assert!(!files.is_empty());
    assert!(files[0].get("_explain").is_none());
    assert!(result.get("_explain").is_none());
}

#[test]
fn map_explain_true_has_breakdown() {
    let (_dir, db) = setup_db();
    let result = map::handle(&db, None, None, true).unwrap();

    assert!(result["_explain"]["formula"].is_string());

    let files = result["files"].as_array().unwrap();
    assert!(!files.is_empty());
    let entry = &files[0];
    let explain = &entry["_explain"];
    assert!(explain.is_object(), "_explain must be present on each file");

    let breakdown = &explain["importance_breakdown"];
    assert!(breakdown["symbol_count"].is_number());
    assert!(breakdown["fan_in_boost"].is_number());
    assert!(breakdown["blast_radius"].is_number());
    assert!(breakdown["pagerank_boost"].is_number());
    assert!(breakdown["complexity_boost"].is_number());

    // Verify breakdown sums to importance
    let importance = entry["importance"].as_i64().unwrap();
    let sum = breakdown["symbol_count"].as_i64().unwrap()
        + breakdown["fan_in_boost"].as_i64().unwrap()
        + breakdown["blast_radius"].as_i64().unwrap()
        + breakdown["pagerank_boost"].as_i64().unwrap()
        + breakdown["complexity_boost"].as_i64().unwrap();
    assert_eq!(importance, sum);
}

#[test]
fn impact_explain_false_has_no_explain_key() {
    let (_dir, db) = setup_db();
    let result = impact::handle(
        &db,
        "a::target_fn",
        false,
        None,
        std::path::Path::new("test"),
    )
    .unwrap();
    assert!(result.get("_explain").is_none());
}

#[test]
fn impact_explain_true_has_frontier_and_thresholds() {
    let (_dir, db) = setup_db();
    let result = impact::handle(
        &db,
        "a::target_fn",
        true,
        None,
        std::path::Path::new("test"),
    )
    .unwrap();

    let explain = &result["_explain"];
    assert!(explain.is_object());

    let frontier = &explain["bfs_frontier"];
    assert!(frontier["depth_1"].is_array());
    assert!(frontier["depth_2"].is_array());
    assert!(frontier["depth_3"].is_array());

    let thresholds = &explain["thresholds"];
    assert_eq!(thresholds["high"]["direct_callers"], 15);
    assert_eq!(thresholds["high"]["files_touched"], 20);
    assert_eq!(thresholds["medium"]["direct_callers"], 5);
    assert_eq!(thresholds["medium"]["files_touched"], 8);
}

#[test]
fn pr_risk_explain_true_has_ceilings_and_contributions() {
    let (_dir, db) = setup_db();
    let changed = vec!["src/a.rs".to_string()];
    let result = pr_risk::compute(&db, &changed, &Default::default(), true).unwrap();

    let explain = &result["_explain"];
    assert!(explain.is_object());
    assert!(explain["formula"].is_string());
    assert!(explain["riskiest_symbols_formula"].is_string());

    let signals = &explain["signals"];
    for key in &["blast_radius", "complexity", "churn", "volume"] {
        let sig = &signals[key];
        assert!(sig["ceiling"].is_number(), "{key} must have ceiling");
        assert!(
            sig["contribution"].is_number(),
            "{key} must have contribution"
        );
    }
}

#[test]
fn pr_risk_explain_false_has_no_explain_key() {
    let (_dir, db) = setup_db();
    let changed = vec!["src/a.rs".to_string()];
    let result = pr_risk::compute(&db, &changed, &Default::default(), false).unwrap();
    assert!(result.get("_explain").is_none());
}

#[test]
fn review_explain_true_has_weights() {
    let (_dir, db) = setup_db();
    let changed = vec!["src/a.rs".to_string()];
    let findings = ReviewFindings::default();
    let result = review::compute(
        &db,
        std::path::Path::new("/tmp"),
        &changed,
        &Default::default(),
        &findings,
        true,
    )
    .unwrap();

    let explain = &result["_explain"];
    assert!(explain.is_object());
    assert!(explain["formula"].is_string());

    let weights = &explain["weights"];
    for key in &[
        "blast_radius",
        "complexity",
        "hotspot_overlap",
        "churn",
        "deviations",
    ] {
        let w = &weights[key];
        assert!(w["weight"].is_number(), "{key} must have weight");
        assert!(w["ceiling"].is_number(), "{key} must have numeric ceiling");
        assert!(
            w["contribution"].is_number(),
            "{key} must have contribution"
        );
        assert!(w["rationale"].is_string(), "{key} must have rationale");
    }
}

#[test]
fn review_explain_false_has_no_explain_key() {
    let (_dir, db) = setup_db();
    let changed = vec!["src/a.rs".to_string()];
    let findings = ReviewFindings::default();
    let result = review::compute(
        &db,
        std::path::Path::new("/tmp"),
        &changed,
        &Default::default(),
        &findings,
        false,
    )
    .unwrap();
    assert!(result.get("_explain").is_none());
}

#[test]
fn file_health_explain_true_has_categories_and_findings() {
    let (_dir, db) = setup_db();
    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();

    let findings = vec![HealthFinding {
        file_id: fa.id,
        symbol_id: None,
        biomarker_kind: BiomarkerKind::NestedComplexity,
        severity: HealthSeverity::Advisory,
        confidence: 0.9,
        provenance: "test".to_string(),
        metric_value: 8.0,
        threshold: 5.0,
        detail: "high nesting".to_string(),
    }];
    db.replace_health_findings(&findings).unwrap();

    let result = file_health::handle(&db, None, None, None, None, true).unwrap();
    let files = result["files"].as_array().unwrap();
    assert!(!files.is_empty());

    let entry = &files[0];
    let explain = &entry["_explain"];
    assert!(explain.is_object(), "_explain must be present");
    assert!(explain["formula"].is_string());
    assert!(explain["categories"].is_object());
    assert!(explain["findings"].is_array());

    let findings_explain = explain["findings"].as_array().unwrap();
    assert!(!findings_explain.is_empty());
    let f = &findings_explain[0];
    assert!(f["biomarker"].is_string());
    assert!(f["raw_deduction"].is_number());
    assert!(f["scaled_deduction"].is_number());
    assert!(f["scale_factor"].is_number());
}

#[test]
fn file_health_explain_false_has_no_explain_key() {
    let (_dir, db) = setup_db();
    let result = file_health::handle(&db, None, None, Some("all"), None, false).unwrap();
    let files = result["files"].as_array().unwrap();
    if !files.is_empty() {
        assert!(files[0].get("_explain").is_none());
    }
}

#[test]
fn pr_risk_explain_true_empty_diff_has_explain() {
    let (_dir, db) = setup_db();
    let result = pr_risk::compute(&db, &[], &Default::default(), true).unwrap();
    assert_eq!(result["composite_score"], 0.0);
    let explain = &result["_explain"];
    assert!(
        explain.is_object(),
        "_explain must be present on empty diff"
    );
    assert!(explain["formula"].is_string());
    for key in &["blast_radius", "complexity", "churn", "volume"] {
        let sig = &explain["signals"][key];
        assert!(sig["ceiling"].is_number(), "{key} must have ceiling");
        assert_eq!(sig["contribution"], 0.0, "{key} contribution must be 0");
    }
}

#[test]
fn review_explain_true_empty_diff_has_explain() {
    let (_dir, db) = setup_db();
    let findings = ReviewFindings::default();
    let result = review::compute(
        &db,
        std::path::Path::new("/tmp"),
        &[],
        &Default::default(),
        &findings,
        true,
    )
    .unwrap();
    assert_eq!(result["risk_score"], 0.0);
    let explain = &result["_explain"];
    assert!(
        explain.is_object(),
        "_explain must be present on empty diff"
    );
    assert!(explain["formula"].is_string());
    for key in &[
        "blast_radius",
        "complexity",
        "hotspot_overlap",
        "churn",
        "deviations",
    ] {
        let w = &explain["weights"][key];
        assert!(w["weight"].is_number(), "{key} must have weight");
        assert!(w["ceiling"].is_number(), "{key} must have numeric ceiling");
        assert_eq!(w["contribution"], 0.0, "{key} contribution must be 0");
    }
}

#[test]
fn review_explain_hotspot_ceiling_is_numeric() {
    let (_dir, db) = setup_db();
    let changed = vec!["src/a.rs".to_string()];
    let findings = ReviewFindings::default();
    let result = review::compute(
        &db,
        std::path::Path::new("/tmp"),
        &changed,
        &Default::default(),
        &findings,
        true,
    )
    .unwrap();
    let ceiling = &result["_explain"]["weights"]["hotspot_overlap"]["ceiling"];
    assert!(
        ceiling.is_number(),
        "hotspot ceiling must be numeric, got {ceiling}"
    );
    assert_eq!(ceiling.as_f64().unwrap(), 1.0);
}
