use sutra::db::{Db, InsertSymbolParams};
use sutra::health::{
    compute_all_health_findings, compute_nested_complexity, BiomarkerKind, HealthSeverity,
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
