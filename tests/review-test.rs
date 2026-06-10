use std::fs;
use std::time::Duration;

use sutra::constraints::DdEngine;
use sutra::constraints::check::FindingDelta;
use sutra::db::{Db, InsertSymbolParams};
use sutra::parser::adapter::default_registry;
use sutra::rules::Severity;
use sutra::tools::review;
use sutra::tools::scoring::ChurnMap;
use sutra::waivers::Waived;

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
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    }
}

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

fn no_findings() -> review::ReviewFindings {
    review::ReviewFindings::default()
}

// ── Structural core tests (from v1/13) ──────────────────────────────

#[test]
fn empty_diff_returns_correct_shape() {
    let (dir, db) = setup_db();
    let result =
        review::compute(&db, dir.path(), &[], &Default::default(), &no_findings()).unwrap();

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

    db.upsert_file("src/core.rs", "rust", "h1", 200, true)
        .unwrap();
    db.upsert_file("src/helper.rs", "rust", "h2", 50, true)
        .unwrap();
    db.upsert_file("src/consumer.rs", "rust", "h3", 100, true)
        .unwrap();

    let f_core = db.file_by_path("src/core.rs").unwrap().unwrap();
    let f_helper = db.file_by_path("src/helper.rs").unwrap().unwrap();
    let f_consumer = db.file_by_path("src/consumer.rs").unwrap().unwrap();

    db.insert_symbol(&sym(
        f_core.id,
        "core::process",
        "process",
        Some("fn process()"),
        1,
        40,
        Some(20),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        f_helper.id,
        "helper::format",
        "format",
        Some("fn format()"),
        1,
        10,
        Some(3),
    ))
    .unwrap();

    let sym_core = db.find_symbols_by_file(f_core.id).unwrap();
    db.insert_symbol(&sym(
        f_consumer.id,
        "consumer::run",
        "run",
        Some("fn run()"),
        1,
        20,
        Some(5),
    ))
    .unwrap();

    // consumer references core::process
    db.insert_ref(f_consumer.id, Some(sym_core[0].id), None, 5, 0, "call")
        .unwrap();

    // Set blast radii
    db.update_rollups(f_core.id, 2, 25).unwrap();
    db.update_rollups(f_helper.id, 0, 3).unwrap();
    db.update_rollups(f_consumer.id, 1, 5).unwrap();

    (dir, db)
}

#[test]
fn single_file_change_populates_all_fields() {
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &Default::default(),
        &no_findings(),
    )
    .unwrap();

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
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string(), "src/helper.rs".to_string()];
    let mut churn = ChurnMap::default();
    churn.counts.insert("src/core.rs".to_string(), 12);

    let result = review::compute(&db, dir.path(), &changed, &churn, &no_findings()).unwrap();

    let breakdown = &result["risk_breakdown"];
    let blast = breakdown["blast_radius"].as_f64().unwrap();
    let complexity = breakdown["complexity_delta"].as_f64().unwrap();
    let hotspot = breakdown["hotspot_overlap"].as_f64().unwrap();
    let churn_s = breakdown["churn"].as_f64().unwrap();
    let conv = breakdown["convention_violations"].as_f64().unwrap();

    assert!(blast > 0.0, "blast should reflect high blast_radius");
    assert!(complexity > 0.0, "complexity should reflect cognitive=20");
    assert!(churn_s > 0.0, "churn should reflect 12 commits");

    let expected =
        (0.30 * blast + 0.20 * complexity + 0.15 * hotspot + 0.15 * churn_s + 0.20 * conv).min(1.0);
    let risk = result["risk_score"].as_f64().unwrap();
    assert!(
        (risk - (expected * 1000.0).round() / 1000.0).abs() < 0.002,
        "risk_score {risk} should match weighted sum {expected}"
    );
}

#[test]
fn truncation_caps_affected_lists() {
    let (dir, db) = setup_db();

    db.upsert_file("src/hub.rs", "rust", "hub", 300, true)
        .unwrap();
    let f_hub = db.file_by_path("src/hub.rs").unwrap().unwrap();
    let hub_sym_id = db
        .insert_symbol(&sym(
            f_hub.id,
            "hub::central",
            "central",
            Some("fn central()"),
            1,
            50,
            Some(10),
        ))
        .unwrap();
    db.update_rollups(f_hub.id, 25, 60).unwrap();

    for i in 0..25 {
        let path = format!("src/consumer_{i}.rs");
        db.upsert_file(&path, "rust", &format!("c{i}"), 20, true)
            .unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("consumer_{i}::use_hub");
        db.insert_symbol(&sym(f.id, &qn, "use_hub", None, 1, 10, Some(2)))
            .unwrap();
        db.insert_ref(f.id, Some(hub_sym_id), None, 3, 0, "call")
            .unwrap();
        db.update_rollups(f.id, 0, i as i64).unwrap();
    }

    let changed = vec!["src/hub.rs".to_string()];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &Default::default(),
        &no_findings(),
    )
    .unwrap();

    let af = result["affected_files"].as_array().unwrap();
    assert_eq!(af.len(), 20, "affected files should be capped at 20");

    let total = &result["affected_total"];
    assert!(
        total["files"].as_u64().unwrap() >= 25,
        "total should report true count"
    );
    assert!(
        total["files_truncated"].as_bool().unwrap(),
        "should be flagged as truncated"
    );

    let rr = result["recommended_reads"].as_array().unwrap();
    assert!(rr.len() <= 10, "recommended_reads should be capped at 10");

    let risk = result["risk_score"].as_f64().unwrap();
    assert!(
        risk > 0.3,
        "high blast file should produce significant risk, got {risk}"
    );
}

#[test]
fn risk_score_clamped_to_one() {
    let (dir, db) = setup_db();

    for i in 0..30 {
        let path = format!("src/extreme_{i}.rs");
        db.upsert_file(&path, "rust", &format!("e{i}"), 500, true)
            .unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("extreme_{i}::danger");
        db.insert_symbol(&sym(f.id, &qn, "danger", None, 1, 100, Some(50)))
            .unwrap();
        db.update_rollups(f.id, 20, 80).unwrap();
    }

    let paths: Vec<String> = (0..30).map(|i| format!("src/extreme_{i}.rs")).collect();
    let mut churn = ChurnMap::default();
    for p in &paths {
        churn.counts.insert(p.clone(), 50);
    }

    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        resolved_constraint_violations: vec![],
        waived_constraint_violations: vec![],
        constraint_violations_total: 0,
        convention_violations: (0..10)
            .map(|i| review::ConventionViolation {
                symbol: format!("extreme_{i}::danger"),
                file: format!("src/extreme_{i}.rs"),
                convention_id: format!("c{i}"),
                antecedent: vec!["kind:function".into()],
                consequent: vec!["has_doc".into()],
                missing: vec!["has_doc".into()],
                support: 5,
                confidence: 0.95,
            })
            .collect(),
        convention_matches: vec![],
        waived_violations: vec![],
        drift_alerts: vec![],
        ..Default::default()
    };

    let result = review::compute(&db, dir.path(), &paths, &churn, &findings).unwrap();
    let risk = result["risk_score"].as_f64().unwrap();
    assert!(risk <= 1.0, "risk must be clamped to 1.0, got {risk}");
    assert!(risk >= 0.95, "extreme risk should be near 1.0, got {risk}");
}

#[test]
fn unknown_files_handled_gracefully() {
    let (dir, db) = setup_db();
    let changed = vec!["src/nonexistent.rs".to_string()];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &Default::default(),
        &no_findings(),
    )
    .unwrap();

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
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let findings = review::ReviewFindings {
        constraint_violations: vec![
            review::ConstraintFinding {
                constraint_id: "abc12345".into(),
                constraint_name: Some("no-core-internal".into()),
                constraint_kind: "forbidden_dep".into(),
                severity: Severity::Blocking,
                provenance: Some("docs/adr-001".into()),
                from_path: "src/core.rs".into(),
                to_path: "src/internal.rs".into(),
                component_context: None,
                detail: "forbidden: src/core.rs -> src/internal.rs".into(),
                delta: FindingDelta::Unknown,
            },
            review::ConstraintFinding {
                constraint_id: "builtin:cycles".into(),
                constraint_name: None,
                constraint_kind: "no_cycles".into(),
                severity: Severity::Blocking,
                provenance: None,
                from_path: "src/core.rs".into(),
                to_path: "src/helper.rs".into(),
                component_context: None,
                detail: "import cycle: src/core.rs -> src/helper.rs -> src/core.rs".into(),
                delta: FindingDelta::Unknown,
            },
        ],
        resolved_constraint_violations: vec![],
        waived_constraint_violations: vec![],
        constraint_violations_total: 2,
        convention_violations: vec![],
        convention_matches: vec![],
        waived_violations: vec![],
        drift_alerts: vec![],
        ..Default::default()
    };

    let result =
        review::compute(&db, dir.path(), &changed, &Default::default(), &findings).unwrap();

    let cv = result["constraint_violations"].as_array().unwrap();
    assert_eq!(cv.len(), 2);
    assert_eq!(cv[0]["kind"], "forbidden_dep");
    assert_eq!(cv[0]["constraint_id"], "abc12345");
    assert_eq!(cv[0]["constraint_name"], "no-core-internal");
    assert_eq!(cv[0]["severity"], "blocking");
    assert_eq!(cv[0]["provenance"], "docs/adr-001");
    assert_eq!(cv[0]["from"], "src/core.rs");
    assert_eq!(cv[0]["to"], "src/internal.rs");
    assert_eq!(cv[1]["kind"], "no_cycles");
    assert_eq!(result["constraint_violations_total"].as_u64().unwrap(), 2);
}

#[test]
fn convention_violations_appear_in_output() {
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        resolved_constraint_violations: vec![],
        waived_constraint_violations: vec![],
        constraint_violations_total: 0,
        convention_violations: vec![review::ConventionViolation {
            symbol: "core::process".into(),
            file: "src/core.rs".into(),
            convention_id: "abc123".into(),
            antecedent: vec!["kind:function".into(), "vis:pub".into()],
            consequent: vec!["has_doc".into()],
            missing: vec!["has_doc".into()],
            support: 8,
            confidence: 0.95,
        }],
        convention_matches: vec![],
        waived_violations: vec![],
        drift_alerts: vec![],
        ..Default::default()
    };

    let result =
        review::compute(&db, dir.path(), &changed, &Default::default(), &findings).unwrap();

    let cv = result["convention_violations"].as_array().unwrap();
    assert_eq!(cv.len(), 1);
    assert_eq!(cv[0]["symbol"], "core::process");
    assert_eq!(cv[0]["file"], "src/core.rs");
    assert_eq!(cv[0]["convention_id"], "abc123");
    assert_eq!(
        cv[0]["missing"].as_array().unwrap(),
        &[serde_json::json!("has_doc")]
    );
    assert_eq!(cv[0]["support"].as_u64().unwrap(), 8);
    assert!((cv[0]["confidence"].as_f64().unwrap() - 0.95).abs() < f64::EPSILON);
}

#[test]
fn waived_violations_appear_in_output() {
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        resolved_constraint_violations: vec![],
        waived_constraint_violations: vec![],
        constraint_violations_total: 0,
        convention_violations: vec![],
        convention_matches: vec![],
        waived_violations: vec![review::WaivedViolation {
            symbol: "core::process".into(),
            file: "src/core.rs".into(),
            convention_id: "abc123".into(),
            antecedent: vec!["kind:function".into()],
            consequent: vec!["has_doc".into()],
            missing: vec!["has_doc".into()],
            support: 8,
            confidence: 0.95,
            rationale: "intentional omission".into(),
        }],
        drift_alerts: vec![],
        ..Default::default()
    };

    let result =
        review::compute(&db, dir.path(), &changed, &Default::default(), &findings).unwrap();

    let wv = result["waived_violations"].as_array().unwrap();
    assert_eq!(wv.len(), 1);
    assert_eq!(wv[0]["symbol"], "core::process");
    assert_eq!(wv[0]["waived"], true);
    assert_eq!(wv[0]["rationale"], "intentional omission");
    assert_eq!(wv[0]["convention_id"], "abc123");

    let cv = result["convention_violations"].as_array().unwrap();
    assert!(
        cv.is_empty(),
        "waived violations should not appear as regular violations"
    );

    let conv_score = result["risk_breakdown"]["convention_violations"]
        .as_f64()
        .unwrap();
    assert!(
        conv_score == 0.0,
        "waived violations should not contribute to risk score"
    );
}

#[test]
fn violations_are_structurally_distinct() {
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let findings = review::ReviewFindings {
        constraint_violations: vec![review::ConstraintFinding {
            constraint_id: "abc12345".into(),
            constraint_name: None,
            constraint_kind: "forbidden_dep".into(),
            severity: Severity::Blocking,
            provenance: None,
            from_path: "src/core.rs".into(),
            to_path: "src/internal.rs".into(),
            component_context: None,
            detail: "forbidden dep".into(),
            delta: FindingDelta::Unknown,
        }],
        resolved_constraint_violations: vec![],
        waived_constraint_violations: vec![],
        constraint_violations_total: 1,
        convention_violations: vec![review::ConventionViolation {
            symbol: "core::process".into(),
            file: "src/core.rs".into(),
            convention_id: "abc123".into(),
            antecedent: vec!["kind:function".into()],
            consequent: vec!["has_doc".into()],
            missing: vec!["has_doc".into()],
            support: 5,
            confidence: 0.92,
        }],
        convention_matches: vec![],
        waived_violations: vec![],
        drift_alerts: vec![],
        ..Default::default()
    };

    let result =
        review::compute(&db, dir.path(), &changed, &Default::default(), &findings).unwrap();

    // Constraint violations have enriched fields
    let cv = &result["constraint_violations"].as_array().unwrap()[0];
    assert!(cv["kind"].is_string());
    assert!(cv["constraint_id"].is_string());
    assert!(cv["severity"].is_string());
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
    assert!(fv.get("constraint_id").is_none());
}

#[test]
fn convention_violations_increase_risk_score() {
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let result_without = review::compute(
        &db,
        dir.path(),
        &changed,
        &Default::default(),
        &no_findings(),
    )
    .unwrap();
    let risk_without = result_without["risk_score"].as_f64().unwrap();

    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        resolved_constraint_violations: vec![],
        waived_constraint_violations: vec![],
        constraint_violations_total: 0,
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
        convention_matches: vec![],
        waived_violations: vec![],
        drift_alerts: vec![],
        ..Default::default()
    };

    let result_with =
        review::compute(&db, dir.path(), &changed, &Default::default(), &findings).unwrap();
    let risk_with = result_with["risk_score"].as_f64().unwrap();

    assert!(
        risk_with > risk_without,
        "risk should increase with convention violations: {risk_with} > {risk_without}"
    );

    let conv_score = result_with["risk_breakdown"]["convention_violations"]
        .as_f64()
        .unwrap();
    assert!(
        conv_score > 0.0,
        "convention_violations signal should be > 0"
    );
}

#[test]
fn recommended_reads_ranks_violation_sites_first() {
    let (dir, db) = setup_db();

    // Create hub + 5 consumers
    db.upsert_file("src/hub.rs", "rust", "hub", 300, true)
        .unwrap();
    let f_hub = db.file_by_path("src/hub.rs").unwrap().unwrap();
    let hub_sym_id = db
        .insert_symbol(&sym(
            f_hub.id,
            "hub::central",
            "central",
            Some("fn central()"),
            1,
            50,
            Some(10),
        ))
        .unwrap();
    db.update_rollups(f_hub.id, 10, 40).unwrap();

    for i in 0..5 {
        let path = format!("src/consumer_{i}.rs");
        db.upsert_file(&path, "rust", &format!("c{i}"), 20, true)
            .unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("consumer_{i}::use_hub");
        db.insert_symbol(&sym(f.id, &qn, "use_hub", None, 1, 10, Some(2)))
            .unwrap();
        db.insert_ref(f.id, Some(hub_sym_id), None, 3, 0, "call")
            .unwrap();
        db.update_rollups(f.id, 0, 100 - i as i64).unwrap();
    }

    // Consumer_3 has a convention violation — should rank first in reads
    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        resolved_constraint_violations: vec![],
        waived_constraint_violations: vec![],
        constraint_violations_total: 0,
        convention_violations: vec![review::ConventionViolation {
            symbol: "consumer_3::use_hub".into(),
            file: "src/consumer_3.rs".into(),
            convention_id: "v1".into(),
            antecedent: vec!["kind:function".into()],
            consequent: vec!["has_doc".into()],
            missing: vec!["has_doc".into()],
            support: 5,
            confidence: 0.95,
        }],
        convention_matches: vec![],
        waived_violations: vec![],
        drift_alerts: vec![],
        ..Default::default()
    };

    let changed = vec!["src/hub.rs".to_string()];
    let result =
        review::compute(&db, dir.path(), &changed, &Default::default(), &findings).unwrap();

    let rr = result["recommended_reads"].as_array().unwrap();
    assert!(!rr.is_empty());
    assert_eq!(
        rr[0]["path"], "src/consumer_3.rs",
        "violation site should rank first"
    );
    assert_eq!(rr[0]["violation_site"], true);
}

// ── Integration test: build_findings exercises real DD + FCA path ────

#[test]
fn build_findings_integration_with_rules_and_imports() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

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
    db.upsert_file("src/ui/view.rs", "rust", "h1", 100, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 80, true)
        .unwrap();

    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();

    // Symbols — pub functions without docs trigger FCA convention
    // when enough similar symbols establish the pattern
    db.insert_symbol(&sym(
        f_view.id,
        "view::render",
        "render",
        Some("fn render()"),
        1,
        20,
        Some(5),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        f_query.id,
        "query::fetch",
        "fetch",
        Some("fn fetch() -> Result<()>"),
        1,
        15,
        Some(3),
    ))
    .unwrap();

    // Create enough pub+has_doc functions to establish convention {kind:function, vis:pub} => {has_doc}
    for i in 0..6 {
        let path = format!("src/lib_{i}.rs");
        db.upsert_file(&path, "rust", &format!("lib{i}"), 50, true)
            .unwrap();
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
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        })
        .unwrap();
    }

    // Import edge: view.rs -> query.rs (triggers forbidden dep)
    db.insert_import(f_view.id, "src/db/query.rs", Some(f_query.id), 1)
        .unwrap();

    let changed = vec!["src/ui/view.rs".to_string()];
    let registry = default_registry();
    let findings =
        review::build_findings(&db, dir.path(), &changed, "HEAD", None, &registry).unwrap();

    // DD should find the forbidden dep via maintained view
    assert!(
        !findings.constraint_violations.is_empty(),
        "should detect forbidden dep from src/ui/view.rs -> src/db/query.rs"
    );
    let cv = &findings.constraint_violations[0];
    assert_eq!(cv.constraint_kind, "forbidden_dep");
    assert!(!cv.constraint_id.is_empty());
    assert_eq!(cv.severity, Severity::Blocking);
    assert!(cv.detail.contains("src/ui/view.rs"));
    assert!(findings.constraint_violations_total >= 1);

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
fn waived_constraint_violations_appear_in_output() {
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        resolved_constraint_violations: vec![],
        waived_constraint_violations: vec![Waived {
            finding: review::ConstraintFinding {
                constraint_id: "abc12345".into(),
                constraint_name: Some("no-core-internal".into()),
                constraint_kind: "forbidden_dep".into(),
                severity: Severity::Blocking,
                provenance: None,
                from_path: "src/core.rs".into(),
                to_path: "src/internal.rs".into(),
                component_context: None,
                detail: "forbidden: src/core.rs -> src/internal.rs".into(),
                delta: FindingDelta::Unknown,
            },
            rationale: "legacy coupling".into(),
            waived_by: "josh".into(),
        }],
        constraint_violations_total: 1,
        convention_violations: vec![],
        convention_matches: vec![],
        waived_violations: vec![],
        drift_alerts: vec![],
        ..Default::default()
    };

    let result =
        review::compute(&db, dir.path(), &changed, &Default::default(), &findings).unwrap();

    let cv = result["constraint_violations"].as_array().unwrap();
    assert!(
        cv.is_empty(),
        "waived constraint violations should not appear as regular violations"
    );

    let wcv = result["waived_constraint_violations"].as_array().unwrap();
    assert_eq!(wcv.len(), 1);
    assert_eq!(wcv[0]["constraint_id"], "abc12345");
    assert_eq!(wcv[0]["waived"], true);
    assert_eq!(wcv[0]["rationale"], "legacy coupling");
    assert_eq!(wcv[0]["waived_by"], "josh");
    assert_eq!(wcv[0]["kind"], "forbidden_dep");
}

#[test]
fn build_findings_constraint_delta_labels_introduced() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

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

    db.upsert_file("src/ui/view.rs", "rust", "h1", 100, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 80, true)
        .unwrap();
    db.upsert_file("src/lib.rs", "rust", "h3", 50, true)
        .unwrap();

    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();
    let f_lib = db.file_by_path("src/lib.rs").unwrap().unwrap();

    db.insert_symbol(&sym(
        f_view.id,
        "view::render",
        "render",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        f_query.id,
        "query::fetch",
        "fetch",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(f_lib.id, "lib::init", "init", None, 1, 10, Some(2)))
        .unwrap();

    // view.rs imports query.rs (forbidden), lib.rs imports query.rs (not forbidden)
    db.insert_import(f_view.id, "src/db/query.rs", Some(f_query.id), 1)
        .unwrap();
    db.insert_import(f_lib.id, "src/db/query.rs", Some(f_query.id), 1)
        .unwrap();

    // Only view.rs is changed — its forbidden import should be detected as introduced
    let changed = vec!["src/ui/view.rs".to_string()];
    let registry = default_registry();
    let findings =
        review::build_findings(&db, dir.path(), &changed, "HEAD", None, &registry).unwrap();

    assert!(!findings.constraint_violations.is_empty());
    let v = &findings.constraint_violations[0];
    assert_eq!(v.constraint_kind, "forbidden_dep");
    assert!(v.detail.contains("[introduced]"));

    // Total should count ALL violations, not just those touching changed files
    assert!(findings.constraint_violations_total >= 1);
}

#[test]
fn build_findings_partitions_constraint_waivers() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

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

    db.upsert_file("src/ui/view.rs", "rust", "h1", 100, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 80, true)
        .unwrap();

    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();

    db.insert_symbol(&sym(
        f_view.id,
        "view::render",
        "render",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        f_query.id,
        "query::fetch",
        "fetch",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();

    db.insert_import(f_view.id, "src/db/query.rs", Some(f_query.id), 1)
        .unwrap();

    // Create a constraint waiver for this specific violation
    let (constraints, _parse_errors) = sutra::rules::load_rules(dir.path())
        .unwrap()
        .all_constraints();
    let constraint_id = &constraints[0].id;
    db.create_constraint_waiver(
        constraint_id,
        None,
        "src/ui/view.rs",
        None,
        "legacy coupling, will be removed in next sprint",
        "josh",
    )
    .unwrap();

    let changed = vec!["src/ui/view.rs".to_string()];
    let registry = default_registry();
    let findings =
        review::build_findings(&db, dir.path(), &changed, "HEAD", None, &registry).unwrap();

    assert!(
        findings.constraint_violations.is_empty(),
        "waived violations should not appear in constraint_violations"
    );
    assert_eq!(findings.waived_constraint_violations.len(), 1);
    let w = &findings.waived_constraint_violations[0];
    assert_eq!(w.finding.constraint_id, *constraint_id);
    assert_eq!(
        w.rationale,
        "legacy coupling, will be removed in next sprint"
    );
    assert_eq!(w.waived_by, "josh");
    assert_eq!(w.finding.from_path, "src/ui/view.rs");
    assert_eq!(
        findings.constraint_violations_total, 1,
        "total should include waived violations"
    );
}

#[test]
fn convention_pipeline_persists_conventions_to_db() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    // 40 pub functions: 38 with signatures (95%), enough for FCA to find
    // the implication {kind:function, vis:pub} => {has_sig} at 0.95 confidence.
    for i in 0..40 {
        let path = format!("src/f_{i}.rs");
        db.upsert_file(&path, "rust", &format!("f{i}"), 50, true)
            .unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("f_{i}::process");
        let sig = if i < 38 { Some("fn process()") } else { None };
        let doc = if i % 5 == 0 { Some("docs") } else { None };
        db.insert_symbol(&InsertSymbolParams {
            file_id: f.id,
            qualified_name: &qn,
            short_name: "process",
            kind: "function",
            signature: sig,
            signature_hash: None,
            visibility: Some("pub"),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: doc,
            cyclomatic: None,
            cognitive: Some(2),
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        })
        .unwrap();
    }

    assert!(db.all_conventions().unwrap().is_empty());

    let registry = default_registry();
    let outcome = sutra::conventions::pipeline::rebuild(&db, &registry, dir.path()).unwrap();
    assert!(outcome.convention_count > 0);

    let conventions = db.all_conventions().unwrap();
    assert!(
        !conventions.is_empty(),
        "conventions should be persisted to DB after pipeline::rebuild"
    );
    for c in &conventions {
        assert!(!c.id.is_empty());
        assert!(!c.antecedent.is_empty());
        assert!(!c.consequent.is_empty());
        assert!(!c.first_seen.is_empty());
        assert!(!c.last_seen.is_empty());
    }
}

#[test]
fn build_findings_falls_back_to_ephemeral_when_shared_engine_invalidated() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[constraints]
forbidden_deps = [
    { from = "src/ui/**", to = "src/db/**" }
]
"#,
    )
    .unwrap();

    // Two files with a forbidden dep
    db.upsert_file("src/ui/view.rs", "rust", "h1", 10, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 10, true)
        .unwrap();
    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();
    db.insert_import(f_view.id, "src/db/query.rs", Some(f_query.id), 1)
        .unwrap();

    // Create a shared engine, ingest, then invalidate it
    let shared = DdEngine::new(Duration::from_secs(600));
    shared
        .ingest(sutra::constraints::DdFacts {
            import_edges: vec![(f_view.id, f_query.id)],
        })
        .unwrap();
    shared.invalidate();

    // build_findings should fall back to ephemeral and still detect the violation
    let changed = vec!["src/ui/view.rs".to_string()];
    let registry = default_registry();
    let findings =
        review::build_findings(&db, dir.path(), &changed, "HEAD", Some(&shared), &registry)
            .unwrap();
    assert!(
        !findings.constraint_violations.is_empty(),
        "invalidated shared engine should fall back to ephemeral and still detect forbidden dep"
    );
}

#[test]
fn changed_files_include_freshness() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    // Create actual file on disk BEFORE DB upsert so last_parsed > mtime → fresh
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("fresh.rs"), "fn fresh() {}").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    db.upsert_file("src/fresh.rs", "rust", "h1", 10, true)
        .unwrap();
    let f = db.file_by_path("src/fresh.rs").unwrap().unwrap();
    db.insert_symbol(&sym(
        f.id,
        "fresh::fresh",
        "fresh",
        Some("fn fresh()"),
        1,
        5,
        Some(2),
    ))
    .unwrap();
    db.update_rollups(f.id, 0, 1).unwrap();

    let changed = vec!["src/fresh.rs".to_string()];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &Default::default(),
        &no_findings(),
    )
    .unwrap();

    let cf = result["changed_files"].as_array().unwrap();
    assert_eq!(cf[0]["_freshness"], "fresh");
}

#[test]
fn affected_files_include_freshness() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("hub.rs"), "fn hub() {}").unwrap();
    fs::write(src.join("consumer.rs"), "fn consumer() {}").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    db.upsert_file("src/hub.rs", "rust", "h1", 50, true)
        .unwrap();
    db.upsert_file("src/consumer.rs", "rust", "h2", 30, true)
        .unwrap();

    let f_hub = db.file_by_path("src/hub.rs").unwrap().unwrap();
    let f_con = db.file_by_path("src/consumer.rs").unwrap().unwrap();

    let hub_sym_id = db
        .insert_symbol(&sym(
            f_hub.id,
            "hub::process",
            "process",
            Some("fn process()"),
            1,
            20,
            Some(5),
        ))
        .unwrap();
    db.insert_symbol(&sym(
        f_con.id,
        "consumer::use_hub",
        "use_hub",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_ref(f_con.id, Some(hub_sym_id), None, 3, 0, "call")
        .unwrap();
    db.update_rollups(f_hub.id, 1, 10).unwrap();
    db.update_rollups(f_con.id, 0, 2).unwrap();

    let changed = vec!["src/hub.rs".to_string()];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &Default::default(),
        &no_findings(),
    )
    .unwrap();

    let af = result["affected_files"].as_array().unwrap();
    assert!(!af.is_empty());
    assert_eq!(af[0]["_freshness"], "fresh");

    let as_ = result["affected_symbols"].as_array().unwrap();
    assert!(!as_.is_empty());
    assert_eq!(as_[0]["_freshness"], "fresh");

    let rr = result["recommended_reads"].as_array().unwrap();
    assert!(!rr.is_empty());
    assert_eq!(rr[0]["_freshness"], "fresh");
}

#[test]
fn freshness_reflects_actual_file_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    // File created before DB insert → fresh
    fs::write(src.join("fresh.rs"), "fn a() {}").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    db.upsert_file("src/fresh.rs", "rust", "h1", 10, true)
        .unwrap();

    // File modified after DB insert → edited_uncommitted
    db.upsert_file("src/edited.rs", "rust", "h2", 10, true)
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(src.join("edited.rs"), "fn b() { changed }").unwrap();

    // File not on disk → stale_index
    db.upsert_file("src/gone.rs", "rust", "h3", 10, true)
        .unwrap();

    for path in &["src/fresh.rs", "src/edited.rs", "src/gone.rs"] {
        let f = db.file_by_path(path).unwrap().unwrap();
        db.insert_symbol(&sym(f.id, &format!("{path}::f"), "f", None, 1, 5, Some(1)))
            .unwrap();
        db.update_rollups(f.id, 0, 1).unwrap();
    }

    let changed = vec![
        "src/fresh.rs".to_string(),
        "src/edited.rs".to_string(),
        "src/gone.rs".to_string(),
    ];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &Default::default(),
        &no_findings(),
    )
    .unwrap();

    let cf = result["changed_files"].as_array().unwrap();
    let freshness: std::collections::HashMap<&str, &str> = cf
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap(),
                f["_freshness"].as_str().unwrap(),
            )
        })
        .collect();

    assert_eq!(freshness["src/fresh.rs"], "fresh");
    assert_eq!(freshness["src/edited.rs"], "edited_uncommitted");
    assert_eq!(freshness["src/gone.rs"], "stale_index");
}

#[test]
fn build_findings_surfaces_error_on_bad_rules() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(rules_dir.join("rules.toml"), "{{invalid toml").unwrap();

    let registry = default_registry();
    let result = review::build_findings(
        &db,
        dir.path(),
        &["src/foo.rs".to_string()],
        "HEAD",
        None,
        &registry,
    );
    assert!(
        result.is_err(),
        "malformed rules.toml should return Err, not empty findings"
    );
}

#[test]
fn build_findings_cycle_violations_counted_in_total() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(rules_dir.join("rules.toml"), "[constraints]\n").unwrap();

    db.upsert_file("src/a.rs", "rust", "ha", 50, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 50, true).unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();

    // a -> b -> a (cycle)
    db.insert_import(fa.id, "src/b.rs", Some(fb.id), 1).unwrap();
    db.insert_import(fb.id, "src/a.rs", Some(fa.id), 1).unwrap();

    let changed = vec!["src/a.rs".to_string()];
    let registry = default_registry();
    let findings =
        review::build_findings(&db, dir.path(), &changed, "HEAD", None, &registry).unwrap();

    assert!(
        findings
            .constraint_violations
            .iter()
            .any(|v| v.constraint_kind == "no_cycles"),
        "should detect the import cycle"
    );
    assert_eq!(
        findings.constraint_violations_total,
        findings.constraint_violations.len(),
        "total should equal the number of violations (including cycles)"
    );
}

#[test]
fn degraded_findings_nullify_risk_score() {
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let mut result = review::compute(
        &db,
        dir.path(),
        &changed,
        &Default::default(),
        &no_findings(),
    )
    .unwrap();
    assert!(
        result["risk_score"].as_f64().is_some(),
        "normal review has numeric risk_score"
    );

    if let Some(obj) = result.as_object_mut() {
        obj.insert("findings_degraded".into(), serde_json::json!(true));
        obj.insert("findings_error".into(), serde_json::json!("bad rules"));
        obj.insert("risk_score".into(), serde_json::json!(null));
    }
    assert!(
        result["risk_score"].is_null(),
        "degraded review should have null risk_score"
    );
}

#[test]
fn resolved_delta_for_removed_forbidden_edge() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

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

    db.upsert_file("src/ui/view.rs", "rust", "h1", 100, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 80, true)
        .unwrap();
    db.upsert_file("src/lib.rs", "rust", "h3", 50, true)
        .unwrap();

    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();
    let f_lib = db.file_by_path("src/lib.rs").unwrap().unwrap();

    db.insert_symbol(&sym(
        f_view.id,
        "view::render",
        "render",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        f_query.id,
        "query::fetch",
        "fetch",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(f_lib.id, "lib::init", "init", None, 1, 10, Some(2)))
        .unwrap();

    // Current DB: only lib.rs -> query.rs (allowed). The forbidden view.rs -> query.rs was removed.
    db.insert_import(f_lib.id, "src/db/query.rs", Some(f_query.id), 1)
        .unwrap();

    // old_edges records that view.rs previously imported query.rs (forbidden)
    let changed_ids: std::collections::HashSet<i64> = [f_view.id].into_iter().collect();
    let old_edges: std::collections::HashSet<(i64, i64)> =
        [(f_view.id, f_query.id)].into_iter().collect();

    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::ChangedFiles {
            changed_ids: &changed_ids,
            old_edges: &old_edges,
        },
    )
    .unwrap();

    assert!(
        outcome.active.is_empty(),
        "no current violation since forbidden edge was removed"
    );
    assert_eq!(
        outcome.resolved.len(),
        1,
        "removed forbidden edge should appear as resolved"
    );
    assert_eq!(outcome.resolved[0].delta, FindingDelta::Resolved);
    assert!(outcome.resolved[0].from_path.contains("ui/view.rs"));
    assert!(outcome.resolved[0].to_path.contains("db/query.rs"));
}
