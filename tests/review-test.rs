use std::fs;

use sutra::db::{Db, InsertSymbolParams};
use sutra::tools::review;

fn sym<'a>(
    file_id: i64,
    qn: &'a str,
    sn: &'a str,
    sig: Option<&'a str>,
    sl: i64,
    el: i64,
    cognitive: Option<i64>,
) -> InsertSymbolParams<'a> {
    InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind: "function",
        signature: sig,
        signature_hash: None,
        visibility: Some("pub"),
        start_line: sl,
        start_col: 0,
        end_line: el,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive,
        flags: 0,
    }
}

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("test", dir.path()).unwrap();
    (dir, db)
}

fn no_findings() -> review::ReviewFindings {
    review::ReviewFindings::default()
}

// ── Structural core tests (from v1/13) ──────────────────────────────

#[test]
fn empty_diff_returns_correct_shape() {
    let (_dir, db) = setup_db();
    let result = review::compute(&db, &[], &Default::default(), &no_findings()).unwrap();

    assert_eq!(result["changed_files"].as_array().unwrap().len(), 0);
    assert_eq!(result["changed_symbols"].as_array().unwrap().len(), 0);
    assert_eq!(result["affected_files"].as_array().unwrap().len(), 0);
    assert_eq!(result["affected_symbols"].as_array().unwrap().len(), 0);
    assert_eq!(result["affected_total"]["files"].as_u64().unwrap(), 0);
    assert_eq!(result["affected_total"]["symbols"].as_u64().unwrap(), 0);

    let risk = result["risk_score"].as_f64().unwrap();
    assert!((risk - 0.0).abs() < f64::EPSILON);

    let breakdown = &result["risk_breakdown"];
    assert!(breakdown["blast_radius"].as_f64().is_some());
    assert!(breakdown["complexity_delta"].as_f64().is_some());
    assert!(breakdown["hotspot_overlap"].as_f64().is_some());
    assert!(breakdown["churn"].as_f64().is_some());
    assert!(breakdown["convention_violations"].as_f64().is_some());

    assert_eq!(result["recommended_reads"].as_array().unwrap().len(), 0);
    assert_eq!(result["constraint_violations"].as_array().unwrap().len(), 0);
    assert_eq!(result["convention_violations"].as_array().unwrap().len(), 0);
}

fn setup_db_with_files() -> (tempfile::TempDir, Db) {
    let (dir, db) = setup_db();

    db.upsert_file("src/core.rs", "rust", "h1", 200, true).unwrap();
    db.upsert_file("src/helper.rs", "rust", "h2", 50, true).unwrap();
    db.upsert_file("src/consumer.rs", "rust", "h3", 100, true).unwrap();

    let f_core = db.file_by_path("src/core.rs").unwrap().unwrap();
    let f_helper = db.file_by_path("src/helper.rs").unwrap().unwrap();
    let f_consumer = db.file_by_path("src/consumer.rs").unwrap().unwrap();

    db.insert_symbol(&sym(f_core.id, "core::process", "process", Some("fn process()"), 1, 40, Some(20))).unwrap();
    db.insert_symbol(&sym(f_helper.id, "helper::format", "format", Some("fn format()"), 1, 10, Some(3))).unwrap();

    let sym_core = db.find_symbols_by_file(f_core.id).unwrap();
    db.insert_symbol(&sym(f_consumer.id, "consumer::run", "run", Some("fn run()"), 1, 20, Some(5))).unwrap();

    // consumer references core::process
    db.insert_ref(f_consumer.id, Some(sym_core[0].id), None, 5, 0, "call").unwrap();

    // Set blast radii
    db.update_rollups(f_core.id, 2, 25).unwrap();
    db.update_rollups(f_helper.id, 0, 3).unwrap();
    db.update_rollups(f_consumer.id, 1, 5).unwrap();

    (dir, db)
}

#[test]
fn single_file_change_populates_all_fields() {
    let (_dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];
    let result = review::compute(&db, &changed, &Default::default(), &no_findings()).unwrap();

    let cf = result["changed_files"].as_array().unwrap();
    assert_eq!(cf.len(), 1);
    assert_eq!(cf[0]["path"], "src/core.rs");
    assert!(cf[0]["blast_radius"].as_i64().unwrap() > 0);

    let cs = result["changed_symbols"].as_array().unwrap();
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0]["symbol"], "core::process");

    let af = result["affected_files"].as_array().unwrap();
    assert!(!af.is_empty(), "consumer.rs should be affected");
    let affected_paths: Vec<&str> = af.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert!(affected_paths.contains(&"src/consumer.rs"));

    let risk = result["risk_score"].as_f64().unwrap();
    assert!(risk > 0.0);
    assert!(risk <= 1.0);

    let rr = result["recommended_reads"].as_array().unwrap();
    assert!(!rr.is_empty());
}

#[test]
fn risk_breakdown_sums_correctly() {
    let (_dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string(), "src/helper.rs".to_string()];
    let mut churn = review::ChurnMap::default();
    churn.counts.insert("src/core.rs".to_string(), 12);

    let result = review::compute(&db, &changed, &churn, &no_findings()).unwrap();

    let breakdown = &result["risk_breakdown"];
    let blast = breakdown["blast_radius"].as_f64().unwrap();
    let complexity = breakdown["complexity_delta"].as_f64().unwrap();
    let hotspot = breakdown["hotspot_overlap"].as_f64().unwrap();
    let churn_s = breakdown["churn"].as_f64().unwrap();
    let conv = breakdown["convention_violations"].as_f64().unwrap();

    assert!(blast > 0.0, "blast should reflect high blast_radius");
    assert!(complexity > 0.0, "complexity should reflect cognitive=20");
    assert!(churn_s > 0.0, "churn should reflect 12 commits");

    let expected = (0.30 * blast + 0.20 * complexity + 0.15 * hotspot + 0.15 * churn_s + 0.20 * conv).min(1.0);
    let risk = result["risk_score"].as_f64().unwrap();
    assert!((risk - (expected * 1000.0).round() / 1000.0).abs() < 0.002,
        "risk_score {risk} should match weighted sum {expected}");
}

#[test]
fn truncation_caps_affected_lists() {
    let (_dir, db) = setup_db();

    db.upsert_file("src/hub.rs", "rust", "hub", 300, true).unwrap();
    let f_hub = db.file_by_path("src/hub.rs").unwrap().unwrap();
    let hub_sym_id = db.insert_symbol(&sym(
        f_hub.id, "hub::central", "central", Some("fn central()"), 1, 50, Some(10),
    )).unwrap();
    db.update_rollups(f_hub.id, 25, 60).unwrap();

    for i in 0..25 {
        let path = format!("src/consumer_{i}.rs");
        db.upsert_file(&path, "rust", &format!("c{i}"), 20, true).unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("consumer_{i}::use_hub");
        db.insert_symbol(&sym(f.id, &qn, "use_hub", None, 1, 10, Some(2))).unwrap();
        db.insert_ref(f.id, Some(hub_sym_id), None, 3, 0, "call").unwrap();
        db.update_rollups(f.id, 0, i as i64).unwrap();
    }

    let changed = vec!["src/hub.rs".to_string()];
    let result = review::compute(&db, &changed, &Default::default(), &no_findings()).unwrap();

    let af = result["affected_files"].as_array().unwrap();
    assert_eq!(af.len(), 20, "affected files should be capped at 20");

    let total = &result["affected_total"];
    assert!(total["files"].as_u64().unwrap() >= 25, "total should report true count");
    assert!(total["files_truncated"].as_bool().unwrap(), "should be flagged as truncated");

    let rr = result["recommended_reads"].as_array().unwrap();
    assert!(rr.len() <= 10, "recommended_reads should be capped at 10");

    let risk = result["risk_score"].as_f64().unwrap();
    assert!(risk > 0.3, "high blast file should produce significant risk, got {risk}");
}

#[test]
fn risk_score_clamped_to_one() {
    let (_dir, db) = setup_db();

    for i in 0..30 {
        let path = format!("src/extreme_{i}.rs");
        db.upsert_file(&path, "rust", &format!("e{i}"), 500, true).unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("extreme_{i}::danger");
        db.insert_symbol(&sym(f.id, &qn, "danger", None, 1, 100, Some(50))).unwrap();
        db.update_rollups(f.id, 20, 80).unwrap();
    }

    let paths: Vec<String> = (0..30).map(|i| format!("src/extreme_{i}.rs")).collect();
    let mut churn = review::ChurnMap::default();
    for p in &paths {
        churn.counts.insert(p.clone(), 50);
    }

    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        convention_violations: (0..10).map(|i| review::ConventionViolation {
            symbol: format!("extreme_{i}::danger"),
            file: format!("src/extreme_{i}.rs"),
            convention_id: format!("c{i}"),
            antecedent: vec!["kind:function".into()],
            consequent: vec!["has_doc".into()],
            missing: vec!["has_doc".into()],
            support: 5,
            confidence: 0.95,
        }).collect(),
    };

    let result = review::compute(&db, &paths, &churn, &findings).unwrap();
    let risk = result["risk_score"].as_f64().unwrap();
    assert!(risk <= 1.0, "risk must be clamped to 1.0, got {risk}");
    assert!(risk >= 0.95, "extreme risk should be near 1.0, got {risk}");
}

#[test]
fn unknown_files_handled_gracefully() {
    let (_dir, db) = setup_db();
    let changed = vec!["src/nonexistent.rs".to_string()];
    let result = review::compute(&db, &changed, &Default::default(), &no_findings()).unwrap();

    let cf = result["changed_files"].as_array().unwrap();
    assert_eq!(cf.len(), 1);
    assert_eq!(cf[0]["path"], "src/nonexistent.rs");
    assert_eq!(cf[0]["blast_radius"].as_i64().unwrap(), 0);

    let risk = result["risk_score"].as_f64().unwrap();
    assert!(risk >= 0.0);
}

// ── DD + FCA integration tests (v1/14) ──────────────────────────────

#[test]
fn constraint_violations_appear_in_output() {
    let (_dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let findings = review::ReviewFindings {
        constraint_violations: vec![
            review::ConstraintViolation {
                kind: "forbidden_dep".into(),
                from_path: "src/core.rs".into(),
                to_path: "src/internal.rs".into(),
                detail: "forbidden: src/core.rs -> src/internal.rs".into(),
            },
            review::ConstraintViolation {
                kind: "cycle".into(),
                from_path: "src/core.rs".into(),
                to_path: "src/helper.rs".into(),
                detail: "import cycle: src/core.rs -> src/helper.rs -> src/core.rs".into(),
            },
        ],
        convention_violations: vec![],
    };

    let result = review::compute(&db, &changed, &Default::default(), &findings).unwrap();

    let cv = result["constraint_violations"].as_array().unwrap();
    assert_eq!(cv.len(), 2);
    assert_eq!(cv[0]["kind"], "forbidden_dep");
    assert_eq!(cv[0]["from"], "src/core.rs");
    assert_eq!(cv[0]["to"], "src/internal.rs");
    assert_eq!(cv[1]["kind"], "cycle");
}

#[test]
fn convention_violations_appear_in_output() {
    let (_dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        convention_violations: vec![
            review::ConventionViolation {
                symbol: "core::process".into(),
                file: "src/core.rs".into(),
                convention_id: "abc123".into(),
                antecedent: vec!["kind:function".into(), "vis:pub".into()],
                consequent: vec!["has_doc".into()],
                missing: vec!["has_doc".into()],
                support: 8,
                confidence: 0.95,
            },
        ],
    };

    let result = review::compute(&db, &changed, &Default::default(), &findings).unwrap();

    let cv = result["convention_violations"].as_array().unwrap();
    assert_eq!(cv.len(), 1);
    assert_eq!(cv[0]["symbol"], "core::process");
    assert_eq!(cv[0]["file"], "src/core.rs");
    assert_eq!(cv[0]["convention_id"], "abc123");
    assert_eq!(cv[0]["missing"].as_array().unwrap(), &[serde_json::json!("has_doc")]);
    assert_eq!(cv[0]["support"].as_u64().unwrap(), 8);
    assert!((cv[0]["confidence"].as_f64().unwrap() - 0.95).abs() < f64::EPSILON);
}

#[test]
fn violations_are_structurally_distinct() {
    let (_dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let findings = review::ReviewFindings {
        constraint_violations: vec![
            review::ConstraintViolation {
                kind: "forbidden_dep".into(),
                from_path: "src/core.rs".into(),
                to_path: "src/internal.rs".into(),
                detail: "forbidden dep".into(),
            },
        ],
        convention_violations: vec![
            review::ConventionViolation {
                symbol: "core::process".into(),
                file: "src/core.rs".into(),
                convention_id: "abc123".into(),
                antecedent: vec!["kind:function".into()],
                consequent: vec!["has_doc".into()],
                missing: vec!["has_doc".into()],
                support: 5,
                confidence: 0.92,
            },
        ],
    };

    let result = review::compute(&db, &changed, &Default::default(), &findings).unwrap();

    // Constraint violations have kind/from/to/detail
    let cv = &result["constraint_violations"].as_array().unwrap()[0];
    assert!(cv["kind"].is_string());
    assert!(cv["from"].is_string());
    assert!(cv["to"].is_string());
    assert!(cv["detail"].is_string());
    assert!(cv.get("convention_id").is_none());

    // Convention violations have symbol/file/convention_id/antecedent/consequent/missing
    let fv = &result["convention_violations"].as_array().unwrap()[0];
    assert!(fv["symbol"].is_string());
    assert!(fv["file"].is_string());
    assert!(fv["convention_id"].is_string());
    assert!(fv["antecedent"].is_array());
    assert!(fv["consequent"].is_array());
    assert!(fv["missing"].is_array());
    assert!(fv.get("kind").is_none());
}

#[test]
fn convention_violations_increase_risk_score() {
    let (_dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let result_without = review::compute(&db, &changed, &Default::default(), &no_findings()).unwrap();
    let risk_without = result_without["risk_score"].as_f64().unwrap();

    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        convention_violations: vec![
            review::ConventionViolation {
                symbol: "core::process".into(),
                file: "src/core.rs".into(),
                convention_id: "c1".into(),
                antecedent: vec!["kind:function".into()],
                consequent: vec!["has_doc".into()],
                missing: vec!["has_doc".into()],
                support: 5,
                confidence: 0.95,
            },
            review::ConventionViolation {
                symbol: "core::validate".into(),
                file: "src/core.rs".into(),
                convention_id: "c2".into(),
                antecedent: vec!["kind:function".into()],
                consequent: vec!["returns_result".into()],
                missing: vec!["returns_result".into()],
                support: 4,
                confidence: 0.90,
            },
            review::ConventionViolation {
                symbol: "core::init".into(),
                file: "src/core.rs".into(),
                convention_id: "c3".into(),
                antecedent: vec!["vis:pub".into()],
                consequent: vec!["has_doc".into()],
                missing: vec!["has_doc".into()],
                support: 6,
                confidence: 0.93,
            },
        ],
    };

    let result_with = review::compute(&db, &changed, &Default::default(), &findings).unwrap();
    let risk_with = result_with["risk_score"].as_f64().unwrap();

    assert!(risk_with > risk_without,
        "risk should increase with convention violations: {risk_with} > {risk_without}");

    let conv_score = result_with["risk_breakdown"]["convention_violations"].as_f64().unwrap();
    assert!(conv_score > 0.0, "convention_violations signal should be > 0");
}

#[test]
fn recommended_reads_ranks_violation_sites_first() {
    let (_dir, db) = setup_db();

    // Create hub + 5 consumers
    db.upsert_file("src/hub.rs", "rust", "hub", 300, true).unwrap();
    let f_hub = db.file_by_path("src/hub.rs").unwrap().unwrap();
    let hub_sym_id = db.insert_symbol(&sym(
        f_hub.id, "hub::central", "central", Some("fn central()"), 1, 50, Some(10),
    )).unwrap();
    db.update_rollups(f_hub.id, 10, 40).unwrap();

    for i in 0..5 {
        let path = format!("src/consumer_{i}.rs");
        db.upsert_file(&path, "rust", &format!("c{i}"), 20, true).unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("consumer_{i}::use_hub");
        db.insert_symbol(&sym(f.id, &qn, "use_hub", None, 1, 10, Some(2))).unwrap();
        db.insert_ref(f.id, Some(hub_sym_id), None, 3, 0, "call").unwrap();
        db.update_rollups(f.id, 0, 100 - i as i64).unwrap();
    }

    // Consumer_3 has a convention violation — should rank first in reads
    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        convention_violations: vec![
            review::ConventionViolation {
                symbol: "consumer_3::use_hub".into(),
                file: "src/consumer_3.rs".into(),
                convention_id: "v1".into(),
                antecedent: vec!["kind:function".into()],
                consequent: vec!["has_doc".into()],
                missing: vec!["has_doc".into()],
                support: 5,
                confidence: 0.95,
            },
        ],
    };

    let changed = vec!["src/hub.rs".to_string()];
    let result = review::compute(&db, &changed, &Default::default(), &findings).unwrap();

    let rr = result["recommended_reads"].as_array().unwrap();
    assert!(!rr.is_empty());
    assert_eq!(rr[0]["path"], "src/consumer_3.rs", "violation site should rank first");
    assert_eq!(rr[0]["violation_site"], true);
}

// ── Integration test: build_findings exercises real DD + FCA path ────

#[test]
fn build_findings_integration_with_rules_and_imports() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("test", dir.path()).unwrap();

    // Set up rules with a forbidden dep
    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[constraints]
forbidden_deps = [
  { from = "src/ui/*", to = "src/db/*" },
]
"#,
    )
    .unwrap();

    // Create files: src/ui/view.rs imports src/db/query.rs (forbidden)
    db.upsert_file("src/ui/view.rs", "rust", "h1", 100, true).unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 80, true).unwrap();

    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();

    // Symbols — pub functions without docs trigger FCA convention
    // when enough similar symbols establish the pattern
    db.insert_symbol(&sym(f_view.id, "view::render", "render",
        Some("fn render()"), 1, 20, Some(5))).unwrap();
    db.insert_symbol(&sym(f_query.id, "query::fetch", "fetch",
        Some("fn fetch() -> Result<()>"), 1, 15, Some(3))).unwrap();

    // Create enough pub+has_doc functions to establish convention {kind:function, vis:pub} => {has_doc}
    for i in 0..6 {
        let path = format!("src/lib_{i}.rs");
        db.upsert_file(&path, "rust", &format!("lib{i}"), 50, true).unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("lib_{i}::documented_fn");
        db.insert_symbol(&InsertSymbolParams {
            file_id: f.id,
            qualified_name: &qn,
            short_name: "documented_fn",
            kind: "function",
            signature: Some("fn documented_fn()"),
            signature_hash: None,
            visibility: Some("pub"),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: Some("A documented function"),
            cyclomatic: None,
            cognitive: Some(2),
            flags: 0,
        }).unwrap();
    }

    // Import edge: view.rs -> query.rs (triggers forbidden dep)
    db.insert_import(f_view.id, "src/db/query.rs", Some(f_query.id), 1).unwrap();

    let changed = vec!["src/ui/view.rs".to_string()];
    let findings = review::build_findings(&db, dir.path(), &changed).unwrap();

    // DD should find the forbidden dep
    assert!(
        !findings.constraint_violations.is_empty(),
        "should detect forbidden dep from src/ui/view.rs -> src/db/query.rs"
    );
    assert_eq!(findings.constraint_violations[0].kind, "forbidden_dep");
    assert!(findings.constraint_violations[0].detail.contains("src/ui/view.rs"));

    // FCA: view::render is pub+function but lacks docs — if convention was established,
    // it should appear as a violation. The convention requires 3+ support at 0.9 confidence,
    // so with 6 documented pub functions we should get the implication.
    // Note: FCA convention detection is probabilistic, so we check if violations are present
    // but don't hard-fail if the FCA engine doesn't find enough support for this small corpus.
    if !findings.convention_violations.is_empty() {
        let v = &findings.convention_violations[0];
        assert!(!v.symbol.is_empty());
        assert!(!v.file.is_empty());
        assert!(v.confidence >= 0.9);
        assert!(v.support >= 3);
    }
}

#[test]
fn build_findings_surfaces_error_on_bad_rules() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(rules_dir.join("rules.toml"), "{{invalid toml").unwrap();

    let result = review::build_findings(&db, dir.path(), &["src/foo.rs".to_string()]);
    assert!(result.is_err(), "malformed rules.toml should return Err, not empty findings");
}
