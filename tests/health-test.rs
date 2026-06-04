use sutra::db::{Db, HealthFindingRow, InsertSymbolParams};
use sutra::health::{
    compute_all_health_findings, compute_nested_complexity, score_component, score_file,
    BiomarkerKind, HealthSeverity,
};

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

fn seed_file(db: &Db, path: &str) -> i64 {
    db.upsert_file(path, "rust", "abc123", 100, true).unwrap()
}

fn seed_fn(db: &Db, file_id: i64, qn: &str, sn: &str, max_nesting: Option<i64>) -> i64 {
    db.insert_symbol(&InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind: "function",
        signature: None,
        signature_hash: None,
        visibility: Some("pub"),
        start_line: 1,
        start_col: 0,
        end_line: 10,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: Some(1),
        cognitive: Some(0),
        max_nesting,
        flags: 0,
        language_attrs: None,
    })
    .unwrap()
}

// --- Finding model ---

#[test]
fn biomarker_kind_as_str_roundtrip() {
    assert_eq!(BiomarkerKind::NestedComplexity.as_str(), "nested_complexity");
    assert_eq!(BiomarkerKind::CoChangeScatter.as_str(), "co_change_scatter");
    assert_eq!(BiomarkerKind::HrrShapeChange.as_str(), "hrr_shape_change");
}

#[test]
fn severity_defaults() {
    assert_eq!(
        BiomarkerKind::NestedComplexity.default_severity(),
        HealthSeverity::Advisory
    );
    assert_eq!(
        BiomarkerKind::DeadCodeRatio.default_severity(),
        HealthSeverity::Informational
    );
    assert_eq!(
        BiomarkerKind::OwnershipRisk.default_severity(),
        HealthSeverity::Advisory
    );
}

// --- nested_complexity threshold ---

#[test]
fn nested_complexity_skips_shallow_functions() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/shallow.rs");
    seed_fn(&db, fid, "shallow::foo", "foo", Some(2));
    seed_fn(&db, fid, "shallow::bar", "bar", Some(4));

    let findings = compute_nested_complexity(&db).unwrap();
    assert!(findings.is_empty(), "nesting <= 4 should not produce findings");
}

#[test]
fn nested_complexity_flags_deep_functions() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/deep.rs");
    let shallow_id = seed_fn(&db, fid, "deep::shallow", "shallow", Some(2));
    let deep_id = seed_fn(&db, fid, "deep::nested", "nested", Some(6));

    let findings = compute_nested_complexity(&db).unwrap();
    assert_eq!(findings.len(), 1);
    let f = &findings[0];
    assert_eq!(f.biomarker_kind, BiomarkerKind::NestedComplexity);
    assert_eq!(f.severity, HealthSeverity::Advisory);
    assert_eq!(f.symbol_id, Some(deep_id));
    assert_eq!(f.metric_value, 6.0);
    assert_eq!(f.threshold, 4.0);
    assert!(f.detail.contains("nesting depth 6"));

    let _ = shallow_id;
}

#[test]
fn nested_complexity_ignores_null_nesting() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/no_nesting.rs");
    seed_fn(&db, fid, "no_nesting::strct", "strct", None);

    let findings = compute_nested_complexity(&db).unwrap();
    assert!(findings.is_empty());
}

// --- DB round-trip ---

#[test]
fn findings_stored_and_queryable() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_fn(&db, fid, "lib::complex", "complex", Some(7));

    let findings = compute_all_health_findings(&db).unwrap();
    assert_eq!(findings.len(), 1);
    db.replace_health_findings(&findings).unwrap();

    let rows = db.get_health_findings(None, None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].biomarker_kind, "nested_complexity");
    assert_eq!(rows[0].severity, "advisory");
    assert_eq!(rows[0].metric_value, 7.0);

    let by_file = db.get_health_findings(Some(fid), None).unwrap();
    assert_eq!(by_file.len(), 1);

    let by_kind = db
        .get_health_findings(None, Some("nested_complexity"))
        .unwrap();
    assert_eq!(by_kind.len(), 1);

    let empty = db.get_health_findings(None, Some("co_change_scatter")).unwrap();
    assert!(empty.is_empty());
}

#[test]
fn replace_findings_is_idempotent() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_fn(&db, fid, "lib::deep", "deep", Some(5));

    let findings = compute_all_health_findings(&db).unwrap();
    db.replace_health_findings(&findings).unwrap();
    db.replace_health_findings(&findings).unwrap();

    let rows = db.get_health_findings(None, None).unwrap();
    assert_eq!(rows.len(), 1);
}

// --- Waiver exclusion ---

#[test]
fn waiver_excludes_finding_from_active() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/waived.rs");
    seed_fn(&db, fid, "waived::deep", "deep", Some(8));

    let findings = compute_all_health_findings(&db).unwrap();
    db.replace_health_findings(&findings).unwrap();

    db.create_health_waiver(
        "nested_complexity",
        "src/waived.rs",
        None,
        "known coordinator pattern",
        "josh",
    )
    .unwrap();

    let results = db.get_health_findings_with_waiver_status().unwrap();
    assert_eq!(results.len(), 1);
    let (finding, is_waived) = &results[0];
    assert!(is_waived, "finding should be marked as waived");
    assert_eq!(finding.biomarker_kind, "nested_complexity");
}

#[test]
fn waiver_does_not_affect_different_biomarker() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/mixed.rs");
    seed_fn(&db, fid, "mixed::deep", "deep", Some(5));

    let findings = compute_all_health_findings(&db).unwrap();
    db.replace_health_findings(&findings).unwrap();

    db.create_health_waiver(
        "co_change_scatter",
        "src/mixed.rs",
        None,
        "different kind",
        "josh",
    )
    .unwrap();

    let results = db.get_health_findings_with_waiver_status().unwrap();
    assert_eq!(results.len(), 1);
    let (_finding, is_waived) = &results[0];
    assert!(!is_waived, "waiver for different biomarker should not match");
}

#[test]
fn waiver_crud() {
    let (_dir, db) = setup_db();

    let id = db
        .create_health_waiver("nested_complexity", "src/foo.rs", None, "reason", "josh")
        .unwrap();

    let waivers = db.get_health_waivers().unwrap();
    assert_eq!(waivers.len(), 1);
    assert_eq!(waivers[0].id, id);
    assert_eq!(waivers[0].biomarker_kind, "nested_complexity");
    assert_eq!(waivers[0].file_path, "src/foo.rs");

    let id2 = db
        .create_health_waiver(
            "nested_complexity",
            "src/foo.rs",
            None,
            "updated reason",
            "josh",
        )
        .unwrap();
    assert_eq!(id, id2, "upsert should return same id");

    let waivers = db.get_health_waivers().unwrap();
    assert_eq!(waivers.len(), 1);
    assert_eq!(waivers[0].rationale, "updated reason");

    db.delete_health_waiver(id).unwrap();
    let waivers = db.get_health_waivers().unwrap();
    assert!(waivers.is_empty());
}

// --- Scoring ---

fn make_finding(id: i64, file_id: i64, biomarker: &str, severity: &str) -> HealthFindingRow {
    HealthFindingRow {
        id,
        file_id,
        symbol_id: None,
        biomarker_kind: biomarker.to_string(),
        severity: severity.to_string(),
        confidence: 1.0,
        provenance: "computed".to_string(),
        metric_value: 5.0,
        threshold: 4.0,
        detail: String::new(),
    }
}

#[test]
fn scoring_no_findings_yields_perfect_score() {
    let result = score_file(&[]);
    assert_eq!(result.score, 10.0);
    assert!(result.deductions.is_empty());
}

#[test]
fn scoring_single_advisory_finding() {
    let findings = [make_finding(1, 1, "nested_complexity", "advisory")];
    let result = score_file(&findings);
    // advisory weight 1.0 × biomarker weight 1.34 = 1.34 deduction
    assert!((result.score - 8.66).abs() < 0.01);
    assert_eq!(result.deductions.len(), 1);
    assert!((result.deductions[0].raw_deduction - 1.34).abs() < 0.01);
    assert!((result.deductions[0].scaled_deduction - 1.34).abs() < 0.01);
}

#[test]
fn scoring_informational_deducts_less() {
    let findings = [make_finding(1, 1, "dead_code_ratio", "informational")];
    let result = score_file(&findings);
    // informational weight 0.5 × biomarker weight 0.80 = 0.40 deduction
    assert!((result.score - 9.60).abs() < 0.01);
}

#[test]
fn scoring_category_cap_with_proportional_scaling() {
    // Three advisory nested_complexity findings: 3 × 1.34 = 4.02,
    // exceeds structural cap of 2.5. Scale factor = 2.5/4.02.
    let findings = [
        make_finding(1, 1, "nested_complexity", "advisory"),
        make_finding(2, 1, "nested_complexity", "advisory"),
        make_finding(3, 1, "nested_complexity", "advisory"),
    ];
    let result = score_file(&findings);
    // Total structural deduction capped at 2.5 → score = 7.5
    assert!((result.score - 7.5).abs() < 0.01);
    // All three scaled deductions should be equal and sum to 2.5
    let total: f64 = result.deductions.iter().map(|d| d.scaled_deduction).sum();
    assert!((total - 2.5).abs() < 0.01);
    let first = result.deductions[0].scaled_deduction;
    for d in &result.deductions {
        assert!((d.scaled_deduction - first).abs() < 0.001);
    }
}

#[test]
fn scoring_all_categories_maxed_yields_minimum() {
    // Overload every category past its cap; sum of caps = 11.5 > 9.0
    let findings = [
        // organizational cap 3.5: co_change_scatter ×3 = 5.40 → capped
        make_finding(1, 1, "co_change_scatter", "advisory"),
        make_finding(2, 1, "co_change_scatter", "advisory"),
        make_finding(3, 1, "co_change_scatter", "advisory"),
        // structural cap 2.5: nested_complexity ×3 = 4.02 → capped
        make_finding(4, 1, "nested_complexity", "advisory"),
        make_finding(5, 1, "nested_complexity", "advisory"),
        make_finding(6, 1, "nested_complexity", "advisory"),
        // coupling cap 2.0: hidden_coupling ×3 = 3.00 → capped
        make_finding(7, 1, "hidden_coupling", "advisory"),
        make_finding(8, 1, "hidden_coupling", "advisory"),
        make_finding(9, 1, "hidden_coupling", "advisory"),
        // freshness cap 1.5: code_age_volatility ×3 = 3.30 → capped
        make_finding(10, 1, "code_age_volatility", "advisory"),
        make_finding(11, 1, "code_age_volatility", "advisory"),
        make_finding(12, 1, "code_age_volatility", "advisory"),
        // coverage cap 2.0: dead_code_ratio ×6 info = 6×0.40 = 2.40 → capped
        make_finding(13, 1, "dead_code_ratio", "informational"),
        make_finding(14, 1, "dead_code_ratio", "informational"),
        make_finding(15, 1, "dead_code_ratio", "informational"),
        make_finding(16, 1, "dead_code_ratio", "informational"),
        make_finding(17, 1, "dead_code_ratio", "informational"),
        make_finding(18, 1, "dead_code_ratio", "informational"),
    ];
    let result = score_file(&findings);
    assert!((result.score - 1.0).abs() < 0.01);
}

#[test]
fn scoring_component_nloc_weighted() {
    // file A: score 8.0, 300 lines; file B: score 6.0, 100 lines
    // weighted avg = (8.0×300 + 6.0×100) / 400 = 3000/400 = 7.5
    let scores = [(8.0, 300_i64), (6.0, 100)];
    let result = score_component(&scores);
    assert!((result - 7.5).abs() < 0.01);
}

#[test]
fn scoring_component_empty_is_perfect() {
    let result = score_component(&[]);
    assert_eq!(result, 10.0);
}
