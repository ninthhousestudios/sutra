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

#[test]
fn empty_diff_returns_correct_shape() {
    let (_dir, db) = setup_db();
    let result = review::compute(&db, &[], &Default::default()).unwrap();

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

    assert_eq!(result["recommended_reads"].as_array().unwrap().len(), 0);
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
    let result = review::compute(&db, &changed, &Default::default()).unwrap();

    // Changed files
    let cf = result["changed_files"].as_array().unwrap();
    assert_eq!(cf.len(), 1);
    assert_eq!(cf[0]["path"], "src/core.rs");
    assert!(cf[0]["blast_radius"].as_i64().unwrap() > 0);

    // Changed symbols
    let cs = result["changed_symbols"].as_array().unwrap();
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0]["symbol"], "core::process");

    // Affected files — consumer.rs references core::process
    let af = result["affected_files"].as_array().unwrap();
    assert!(!af.is_empty(), "consumer.rs should be affected");
    let affected_paths: Vec<&str> = af.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert!(affected_paths.contains(&"src/consumer.rs"));

    // Risk score should be > 0 since core.rs has blast=25
    let risk = result["risk_score"].as_f64().unwrap();
    assert!(risk > 0.0);
    assert!(risk <= 1.0);

    // Recommended reads should include affected files
    let rr = result["recommended_reads"].as_array().unwrap();
    assert!(!rr.is_empty());
}

#[test]
fn risk_breakdown_sums_correctly() {
    let (_dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string(), "src/helper.rs".to_string()];
    let mut churn = review::ChurnMap::default();
    churn.counts.insert("src/core.rs".to_string(), 12);

    let result = review::compute(&db, &changed, &churn).unwrap();

    let breakdown = &result["risk_breakdown"];
    let blast = breakdown["blast_radius"].as_f64().unwrap();
    let complexity = breakdown["complexity_delta"].as_f64().unwrap();
    let hotspot = breakdown["hotspot_overlap"].as_f64().unwrap();
    let churn_s = breakdown["churn"].as_f64().unwrap();

    assert!(blast > 0.0, "blast should reflect high blast_radius");
    assert!(complexity > 0.0, "complexity should reflect cognitive=20");
    assert!(churn_s > 0.0, "churn should reflect 12 commits");

    // Weighted sum should match risk_score
    let expected = (0.35 * blast + 0.25 * complexity + 0.20 * hotspot + 0.20 * churn_s).min(1.0);
    let risk = result["risk_score"].as_f64().unwrap();
    assert!((risk - (expected * 1000.0).round() / 1000.0).abs() < 0.002,
        "risk_score {risk} should match weighted sum {expected}");
}

#[test]
fn truncation_caps_affected_lists() {
    let (_dir, db) = setup_db();

    // Create a "core" file that everything depends on
    db.upsert_file("src/hub.rs", "rust", "hub", 300, true).unwrap();
    let f_hub = db.file_by_path("src/hub.rs").unwrap().unwrap();
    let hub_sym_id = db.insert_symbol(&sym(
        f_hub.id, "hub::central", "central", Some("fn central()"), 1, 50, Some(10),
    )).unwrap();
    db.update_rollups(f_hub.id, 25, 60).unwrap();

    // Create 25 consumer files that reference hub::central
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
    let result = review::compute(&db, &changed, &Default::default()).unwrap();

    // Affected files should be capped at 20
    let af = result["affected_files"].as_array().unwrap();
    assert_eq!(af.len(), 20, "affected files should be capped at 20");

    // But total should report the true count
    let total = &result["affected_total"];
    assert!(total["files"].as_u64().unwrap() >= 25, "total should report true count");
    assert!(total["files_truncated"].as_bool().unwrap(), "should be flagged as truncated");

    // Recommended reads capped at 10
    let rr = result["recommended_reads"].as_array().unwrap();
    assert!(rr.len() <= 10, "recommended_reads should be capped at 10");

    // Risk score should be high given blast=60
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

    let result = review::compute(&db, &paths, &churn).unwrap();
    let risk = result["risk_score"].as_f64().unwrap();
    assert!(risk <= 1.0, "risk must be clamped to 1.0, got {risk}");
    assert!(risk >= 0.95, "extreme risk should be near 1.0, got {risk}");
}

#[test]
fn unknown_files_handled_gracefully() {
    let (_dir, db) = setup_db();
    let changed = vec!["src/nonexistent.rs".to_string()];
    let result = review::compute(&db, &changed, &Default::default()).unwrap();

    let cf = result["changed_files"].as_array().unwrap();
    assert_eq!(cf.len(), 1);
    assert_eq!(cf[0]["path"], "src/nonexistent.rs");
    assert_eq!(cf[0]["blast_radius"].as_i64().unwrap(), 0);

    let risk = result["risk_score"].as_f64().unwrap();
    assert!(risk >= 0.0);
}
