#[path = "support/health_run.rs"]
mod health_run;
use health_run::{publish_run_with, publish_seeded_run};
use sutra::db::{
    CommitRow, Db, HealthFindingRow, InsertSymbolParams, SnapshotCompleteness,
    SnapshotComponentMember, SnapshotComponentRow, SnapshotFileRow, SnapshotParams,
};
use sutra::git::parse_blame_porcelain;
use sutra::health::compare::BaselineSelector;
use sutra::health::evidence::{MissingReason, ProducerOutcome, UnsupportedReason};
use sutra::health::findings::HealthFinding;
use sutra::health::scoring::{
    BiomarkerScope, EvidencePart, PERSISTENT_PRODUCERS, ProducerResult, ScoreValue, round2,
};
use sutra::health::{
    BiomarkerKind, FileHealthScore, HealthCategory, HealthSeverity, component_score,
    compute_all_health_findings, compute_blast_radius_churn, compute_change_entropy,
    compute_co_change_scatter, compute_hidden_coupling, compute_nested_complexity,
    compute_ownership_risk, instability::compute_component_instability, score_file,
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
        structural_hash: None,
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

/// A category's deduction for `raw` known debt under the scoring curve.
fn sat(cat: HealthCategory, raw: f64) -> f64 {
    cat.saturation().apply(raw)
}

#[test]
fn biomarker_kind_as_str_roundtrip() {
    assert_eq!(
        BiomarkerKind::NestedComplexity.as_str(),
        "nested_complexity"
    );
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
    assert!(
        findings.is_empty(),
        "nesting <= 4 should not produce findings"
    );
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
    let (dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_fn(&db, fid, "lib::complex", "complex", Some(7));

    let findings = compute_all_health_findings(&db, dir.path()).unwrap();
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

    let empty = db
        .get_health_findings(None, Some("co_change_scatter"))
        .unwrap();
    assert!(empty.is_empty());
}

#[test]
fn replace_findings_is_idempotent() {
    let (dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_fn(&db, fid, "lib::deep", "deep", Some(5));

    let findings = compute_all_health_findings(&db, dir.path()).unwrap();
    db.replace_health_findings(&findings).unwrap();
    db.replace_health_findings(&findings).unwrap();

    let rows = db.get_health_findings(None, None).unwrap();
    assert_eq!(rows.len(), 1);
}

// --- Waiver exclusion ---

#[test]
fn waiver_excludes_finding_from_active() {
    let (dir, db) = setup_db();
    let fid = seed_file(&db, "src/waived.rs");
    seed_fn(&db, fid, "waived::deep", "deep", Some(8));

    let findings = compute_all_health_findings(&db, dir.path()).unwrap();
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
    let (dir, db) = setup_db();
    let fid = seed_file(&db, "src/mixed.rs");
    seed_fn(&db, fid, "mixed::deep", "deep", Some(5));

    let findings = compute_all_health_findings(&db, dir.path()).unwrap();
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
    assert!(
        !is_waived,
        "waiver for different biomarker should not match"
    );
}

#[test]
fn waiver_symbol_scoped_matches_correct_symbol() {
    let (dir, db) = setup_db();
    let fid = seed_file(&db, "src/multi.rs");
    let _s1 = seed_fn(&db, fid, "multi::deep_one", "deep_one", Some(6));
    let _s2 = seed_fn(&db, fid, "multi::deep_two", "deep_two", Some(7));

    let findings = compute_all_health_findings(&db, dir.path()).unwrap();
    db.replace_health_findings(&findings).unwrap();

    // Waive only deep_one, not deep_two
    db.create_health_waiver(
        "nested_complexity",
        "src/multi.rs",
        Some("multi::deep_one"),
        "known pattern",
        "josh",
    )
    .unwrap();

    let results = db.get_health_findings_with_waiver_status().unwrap();
    assert_eq!(results.len(), 2, "both findings should still exist");
    let waived_count = results.iter().filter(|(_, w)| *w).count();
    let unwaived_count = results.iter().filter(|(_, w)| !*w).count();
    assert_eq!(waived_count, 1, "only the matching symbol should be waived");
    assert_eq!(unwaived_count, 1, "the other symbol should remain active");
}

#[test]
fn waiver_file_level_covers_all_symbols() {
    let (dir, db) = setup_db();
    let fid = seed_file(&db, "src/blanket.rs");
    seed_fn(&db, fid, "blanket::a", "a", Some(6));
    seed_fn(&db, fid, "blanket::b", "b", Some(7));

    let findings = compute_all_health_findings(&db, dir.path()).unwrap();
    db.replace_health_findings(&findings).unwrap();

    // File-level waiver (no symbol) should cover all findings in the file
    db.create_health_waiver(
        "nested_complexity",
        "src/blanket.rs",
        None,
        "blanket waive",
        "josh",
    )
    .unwrap();

    let results = db.get_health_findings_with_waiver_status().unwrap();
    assert!(
        results.iter().all(|(_, w)| *w),
        "file-level waiver should cover all symbols"
    );
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

#[test]
fn reconcile_orphaned_health_waivers() {
    let (_dir, db) = setup_db();
    let file_id = seed_file(&db, "src/foo.rs");
    let sym_id = seed_fn(&db, file_id, "foo::bar", "bar", Some(6));

    let findings = vec![HealthFinding {
        file_id,
        symbol_id: Some(sym_id),
        biomarker_kind: BiomarkerKind::NestedComplexity,
        severity: HealthSeverity::Advisory,
        confidence: 0.9,
        provenance: "test".into(),
        metric_value: 6.0,
        threshold: 4.0,
        detail: "nesting 6".into(),
    }];
    db.replace_health_findings(&findings).unwrap();

    db.create_health_waiver("nested_complexity", "src/foo.rs", None, "ok", "josh")
        .unwrap();
    db.create_health_waiver("nested_complexity", "src/gone.rs", None, "stale", "josh")
        .unwrap();

    let orphans = db.reconcile_orphaned_health_waivers().unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].file_path, "src/gone.rs");
}

#[test]
fn reconcile_orphaned_health_waivers_symbol_scoped() {
    let (_dir, db) = setup_db();
    let file_id = seed_file(&db, "src/foo.rs");
    let sym_a = seed_fn(&db, file_id, "foo::alive", "alive", Some(6));
    let _sym_b = seed_fn(&db, file_id, "foo::gone", "gone", Some(7));

    // Only sym_a's finding survives; sym_b's finding is removed.
    let findings = vec![HealthFinding {
        file_id,
        symbol_id: Some(sym_a),
        biomarker_kind: BiomarkerKind::NestedComplexity,
        severity: HealthSeverity::Advisory,
        confidence: 0.9,
        provenance: "test".into(),
        metric_value: 6.0,
        threshold: 4.0,
        detail: "nesting 6".into(),
    }];
    db.replace_health_findings(&findings).unwrap();

    // File-level waiver — should NOT be orphaned (finding still exists in file)
    db.create_health_waiver(
        "nested_complexity",
        "src/foo.rs",
        None,
        "file-level",
        "josh",
    )
    .unwrap();
    // Symbol waiver for the surviving symbol — should NOT be orphaned
    db.create_health_waiver(
        "nested_complexity",
        "src/foo.rs",
        Some("foo::alive"),
        "alive-waiver",
        "josh",
    )
    .unwrap();
    // Symbol waiver for the gone symbol — SHOULD be orphaned
    db.create_health_waiver(
        "nested_complexity",
        "src/foo.rs",
        Some("foo::gone"),
        "stale-waiver",
        "josh",
    )
    .unwrap();

    let orphans = db.reconcile_orphaned_health_waivers().unwrap();
    assert_eq!(orphans.len(), 1);
    assert_eq!(
        orphans[0].symbol_qualified_name.as_deref(),
        Some("foo::gone")
    );
}

// --- Scoring ---

/// An outcome for every file-level biomarker (persistent and on-demand): all
/// `Complete` except the structurally unsupported coverage gradient.
fn complete_outcomes() -> Vec<ProducerResult> {
    BiomarkerKind::ALL
        .into_iter()
        .filter(|k| k.scope() != BiomarkerScope::Component)
        .map(|k| {
            let o = if k == BiomarkerKind::CoverageGradient {
                ProducerOutcome::Unsupported(UnsupportedReason::NoCoverageIngestion)
            } else {
                ProducerOutcome::Complete { finding_count: 0 }
            };
            (k, o)
        })
        .collect()
}

fn with_outcome(kinds: &[BiomarkerKind], outcome: ProducerOutcome) -> Vec<ProducerResult> {
    let mut outcomes = complete_outcomes();
    for (k, o) in outcomes.iter_mut() {
        if kinds.contains(k) {
            *o = outcome;
        }
    }
    outcomes
}

fn score(outcomes: &[ProducerResult], findings: &[HealthFindingRow]) -> FileHealthScore {
    score_file(&[EvidencePart { outcomes, findings }])
}

/// Score with every producer complete; the measured value.
fn measured(findings: &[HealthFindingRow]) -> f64 {
    match score(&complete_outcomes(), findings).value {
        ScoreValue::Measured(s) => s,
        other => panic!("expected a measured score, got {other:?}"),
    }
}

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
    let result = score(&complete_outcomes(), &[]);
    assert_eq!(result.value, ScoreValue::Measured(10.0));
    assert!(result.deductions.is_empty());
    // A clean file with every producer complete is not partial.
    assert!(!result.partial());
    assert!(result.missing.is_empty());
}

// --- "missing analysis is never zero debt" contract ---

#[test]
fn contract_coverage_gradient_is_unsupported_and_surfaced() {
    let result = score(&complete_outcomes(), &[]);
    assert!(
        result
            .unsupported
            .iter()
            .any(|(k, _)| *k == BiomarkerKind::CoverageGradient),
        "an unsupported producer is excluded but reported, not scored as zero debt"
    );
}

const GIT_BIOMARKERS: [BiomarkerKind; 5] = [
    BiomarkerKind::CoChangeScatter,
    BiomarkerKind::ChangeEntropy,
    BiomarkerKind::OwnershipRisk,
    BiomarkerKind::HiddenCoupling,
    BiomarkerKind::BlastRadiusChurn,
];

#[test]
fn contract_git_biomarkers_excluded_when_not_a_repo() {
    // Only a confirmed non-repository makes git producers unsupported: excluded,
    // never worst-cased.
    let outcomes = with_outcome(
        &GIT_BIOMARKERS,
        ProducerOutcome::Unsupported(UnsupportedReason::ConfirmedNonRepository),
    );
    let result = score(&outcomes, &[]);
    assert_eq!(result.value, ScoreValue::Measured(10.0));
    assert_eq!(result.unsupported.len(), 1 + GIT_BIOMARKERS.len());
}

#[test]
fn contract_git_biomarkers_bounded_by_category_caps_on_no_history() {
    // No usable history is missing git analysis, not clean. The lower bound
    // saturates every category a git producer lives in (organizational 3.5,
    // structural 2.5 via blast_radius_churn, coupling 2.0 via hidden_coupling) —
    // not one finding weight per missing producer.
    let outcomes = with_outcome(
        &GIT_BIOMARKERS,
        ProducerOutcome::Missing(MissingReason::NoHistory),
    );
    let result = score(&outcomes, &[]);
    match result.value {
        ScoreValue::Partial { lower, upper } => {
            assert!((lower - 2.0).abs() < 1e-9, "lower {lower}");
            assert_eq!(upper, 10.0);
        }
        other => panic!("expected partial, got {other:?}"),
    }
    assert_eq!(result.missing.len(), GIT_BIOMARKERS.len());
}

#[test]
fn contract_known_debt_narrows_the_bound_it_shares_a_category_with() {
    // Known structural debt counts in the upper bound; the missing blast-radius
    // producer still saturates structural in the lower bound.
    let outcomes = with_outcome(
        &[BiomarkerKind::BlastRadiusChurn],
        ProducerOutcome::Missing(MissingReason::NoHistory),
    );
    let findings = [make_finding(1, 1, "nested_complexity", "advisory")];
    let result = score(&outcomes, &findings);
    match result.value {
        ScoreValue::Partial { lower, upper } => {
            let known = sat(HealthCategory::Structural, 1.34);
            assert!((upper - (10.0 - known)).abs() < 1e-9);
            assert!((lower - 7.5).abs() < 1e-9);
        }
        other => panic!("expected partial, got {other:?}"),
    }
}

#[test]
fn contract_review_and_component_biomarkers_are_not_persistent() {
    for kind in [
        BiomarkerKind::FunctionHotspot,
        BiomarkerKind::CodeAgeVolatility,
        BiomarkerKind::HrrShapeChange,
        BiomarkerKind::ComponentInstability,
    ] {
        assert_ne!(
            kind.scope(),
            BiomarkerScope::Persistent,
            "{} is scored elsewhere, not per-file at parse time",
            kind.as_str()
        );
        assert!(!PERSISTENT_PRODUCERS.contains(&kind));
    }
}

#[test]
fn scoring_single_advisory_finding() {
    let findings = [make_finding(1, 1, "nested_complexity", "advisory")];
    let result = score(&complete_outcomes(), &findings);
    // advisory weight 1.0 × biomarker weight 1.34 = 1.34 raw, saturated to ~1.05
    let known = sat(HealthCategory::Structural, 1.34);
    assert!((result.value.upper() - (10.0 - known)).abs() < 1e-9);
    assert!((result.value.upper() - 8.95).abs() < 0.01);
    assert_eq!(result.deductions.len(), 1);
    assert!((result.deductions[0].raw_deduction - 1.34).abs() < 0.01);
    // A lone finding owns its whole category deduction, and resolving it
    // recovers exactly that.
    assert!((result.deductions[0].scaled_deduction - known).abs() < 1e-9);
    assert!((result.deductions[0].marginal - known).abs() < 1e-9);
}

#[test]
fn scoring_informational_deducts_less() {
    let findings = [make_finding(1, 1, "dead_code_ratio", "informational")];
    // informational weight 0.5 × biomarker weight 0.80 = 0.40 raw
    let known = sat(HealthCategory::Coverage, 0.40);
    assert!((measured(&findings) - (10.0 - known)).abs() < 1e-9);
    let advisory = [make_finding(1, 1, "dead_code_ratio", "advisory")];
    assert!(measured(&findings) > measured(&advisory));
}

#[test]
fn scoring_debt_past_the_cap_saturates_with_proportional_scaling() {
    // Three advisory nested_complexity findings: 3 × 1.34 = 4.02 raw, past
    // the structural cap of 2.5 — deducted below the cap, and still more
    // than two findings deduct.
    let findings = [
        make_finding(1, 1, "nested_complexity", "advisory"),
        make_finding(2, 1, "nested_complexity", "advisory"),
        make_finding(3, 1, "nested_complexity", "advisory"),
    ];
    let result = score(&complete_outcomes(), &findings);
    let known = sat(HealthCategory::Structural, 4.02);
    assert!(known < 2.5 && known > sat(HealthCategory::Structural, 2.68));
    assert!((result.value.upper() - (10.0 - known)).abs() < 1e-9);
    // All three scaled deductions are equal and sum to the category deduction.
    let total: f64 = result.deductions.iter().map(|d| d.scaled_deduction).sum();
    assert!((total - known).abs() < 1e-9);
    let first = result.deductions[0].scaled_deduction;
    for d in &result.deductions {
        assert!((d.scaled_deduction - first).abs() < 0.001);
    }
}

#[test]
fn scoring_all_categories_maxed_yields_minimum() {
    // The caps sum to 11.5 > 9.0, so debt heavy enough in every category
    // clamps at the floor even though no category reaches its cap.
    let kinds = [
        ("co_change_scatter", "advisory"),
        ("nested_complexity", "advisory"),
        ("hidden_coupling", "advisory"),
        ("code_age_volatility", "advisory"),
        ("dead_code_ratio", "informational"),
    ];
    let findings: Vec<_> = kinds
        .iter()
        .flat_map(|&kind| std::iter::repeat_n(kind, 200))
        .enumerate()
        .map(|(i, (kind, severity))| make_finding(i as i64 + 1, 1, kind, severity))
        .collect();
    assert!((measured(&findings) - 1.0).abs() < 1e-9);
}

#[test]
fn scoring_heavy_debt_in_every_category_is_saturated_not_capped() {
    // Three findings past each cap: a hard cap floored this at 1.0; the
    // soft saturation leaves it measurably above the floor.
    let findings = [
        make_finding(1, 1, "co_change_scatter", "advisory"),
        make_finding(2, 1, "co_change_scatter", "advisory"),
        make_finding(3, 1, "co_change_scatter", "advisory"),
        make_finding(4, 1, "nested_complexity", "advisory"),
        make_finding(5, 1, "nested_complexity", "advisory"),
        make_finding(6, 1, "nested_complexity", "advisory"),
        make_finding(7, 1, "hidden_coupling", "advisory"),
        make_finding(8, 1, "hidden_coupling", "advisory"),
        make_finding(9, 1, "hidden_coupling", "advisory"),
        make_finding(10, 1, "code_age_volatility", "advisory"),
        make_finding(11, 1, "code_age_volatility", "advisory"),
        make_finding(12, 1, "code_age_volatility", "advisory"),
    ];
    let expected = 10.0
        - sat(HealthCategory::Organizational, 5.40)
        - sat(HealthCategory::Structural, 4.02)
        - sat(HealthCategory::Coupling, 3.00)
        - sat(HealthCategory::Freshness, 3.30);
    assert!((measured(&findings) - expected).abs() < 1e-9);
    assert!(expected > 1.0);
}

#[test]
fn scoring_component_blends_density_and_count() {
    // file A: score 8.0, 300 lines; file B: score 6.0, 100 lines
    // density = NLOC-weighted mean deduction = (2.0×300 + 4.0×100) / 400 = 2.5
    // count   = 9 · b/(1+b), b = ln(1 + 6.0/5.0) ≈ 3.968 (total debt mass 6.0)
    // score   = 10 − (0.5×2.5 + 0.5×3.968) ≈ 6.77
    let scores = [(8.0, 300_i64), (6.0, 100)];
    let result = component_score(&scores, 0.0);
    assert!((result - 6.77).abs() < 0.01, "got {result}");
}

#[test]
fn scoring_component_empty_is_perfect() {
    let result = component_score(&[], 0.0);
    assert_eq!(result, 10.0);
}

// --- git-organizational biomarkers ---

fn seed_commits(db: &Db, commits: &[CommitRow], pairs: &[(String, i64)]) {
    db.replace_commit_files(commits, pairs).unwrap();
}

#[test]
fn co_change_scatter_fires_at_threshold() {
    let (_dir, db) = setup_db();
    let hub = seed_file(&db, "src/hub.rs");
    let mut partners = Vec::new();
    for i in 0..9 {
        partners.push(seed_file(&db, &format!("src/spoke_{i}.rs")));
    }
    let quiet = seed_file(&db, "src/quiet.rs");

    let now = 1_700_000_000i64;
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    // 9 commits: each touches hub + a unique spoke → 9 distinct partners
    for (c, &spoke) in partners.iter().enumerate() {
        let hash = format!("commit_{c:04}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: now + c as i64 * 86400,
            author: "alice@dev".into(),
        });
        pairs.push((hash.clone(), hub));
        pairs.push((hash, spoke));
    }
    // quiet file: only in 1 commit with 1 partner
    pairs.push(("commit_0000".into(), quiet));

    seed_commits(&db, &commits, &pairs);

    let findings = compute_co_change_scatter(&db).unwrap();
    let hub_findings: Vec<_> = findings.iter().filter(|f| f.file_id == hub).collect();
    assert_eq!(hub_findings.len(), 1);
    assert_eq!(
        hub_findings[0].biomarker_kind,
        BiomarkerKind::CoChangeScatter
    );
    assert!(hub_findings[0].metric_value >= 8.0);
    assert!(hub_findings[0].detail.contains("co-change partners"));

    let quiet_findings: Vec<_> = findings.iter().filter(|f| f.file_id == quiet).collect();
    assert!(quiet_findings.is_empty());
}

#[test]
fn co_change_scatter_requires_minimum_commits() {
    let (_dir, db) = setup_db();
    let hub = seed_file(&db, "src/hub.rs");
    let mut partners = Vec::new();
    for i in 0..10 {
        partners.push(seed_file(&db, &format!("src/p_{i}.rs")));
    }
    // Only 2 commits — below the threshold of 3
    let commits = vec![
        CommitRow {
            hash: "c1".into(),
            committed_at: 1_700_000_000,
            author: "a@b".into(),
        },
        CommitRow {
            hash: "c2".into(),
            committed_at: 1_700_086_400,
            author: "a@b".into(),
        },
    ];
    let mut pairs = Vec::new();
    for (i, &pid) in partners.iter().enumerate() {
        let hash = if i < 5 { "c1" } else { "c2" };
        pairs.push((hash.to_string(), pid));
        pairs.push((hash.to_string(), hub));
    }
    seed_commits(&db, &commits, &pairs);

    let findings = compute_co_change_scatter(&db).unwrap();
    let hub_findings: Vec<_> = findings.iter().filter(|f| f.file_id == hub).collect();
    assert!(hub_findings.is_empty(), "only 2 commits, should not fire");
}

#[test]
fn co_change_scatter_solo_commits_dont_inflate_guard() {
    let (_dir, db) = setup_db();
    let hub = seed_file(&db, "src/hub.rs");
    let mut partners = Vec::new();
    for i in 0..10 {
        partners.push(seed_file(&db, &format!("src/s_{i}.rs")));
    }

    let now = 1_700_000_000i64;
    // 4 solo commits (hub only — no co-change partners)
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..4 {
        let hash = format!("solo_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: now + i * 86400,
            author: "a@b".into(),
        });
        pairs.push((hash, hub));
    }
    // 1 broad commit touching hub + all 10 partners
    commits.push(CommitRow {
        hash: "broad".into(),
        committed_at: now + 5 * 86400,
        author: "a@b".into(),
    });
    pairs.push(("broad".into(), hub));
    for &pid in &partners {
        pairs.push(("broad".into(), pid));
    }

    seed_commits(&db, &commits, &pairs);

    // hub has 10 partners but only 1 co-change commit — guard should reject
    let findings = compute_co_change_scatter(&db).unwrap();
    let hub_findings: Vec<_> = findings.iter().filter(|f| f.file_id == hub).collect();
    assert!(
        hub_findings.is_empty(),
        "solo commits should not inflate the co-change commit guard"
    );
}

#[test]
fn change_entropy_computation() {
    let (_dir, db) = setup_db();
    let f1 = seed_file(&db, "src/busy.rs");
    let f2 = seed_file(&db, "src/other.rs");

    let now = 1_700_000_000i64;
    // 10 commits each touching f1 + f2 (2 files each, recent)
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..10 {
        let hash = format!("e_{i:03}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: now - i * 3600,
            author: "dev@x".into(),
        });
        pairs.push((hash.clone(), f1));
        pairs.push((hash.clone(), f2));
    }
    seed_commits(&db, &commits, &pairs);

    let findings = compute_change_entropy(&db).unwrap();
    // Each commit has F=2, contribution = (1/2) * log2(2) * decay ≈ 0.5 * decay
    // 10 recent commits with minimal decay → sum ≈ 5.0
    let f1_findings: Vec<_> = findings.iter().filter(|f| f.file_id == f1).collect();
    assert_eq!(f1_findings.len(), 1);
    assert_eq!(f1_findings[0].biomarker_kind, BiomarkerKind::ChangeEntropy);
    assert!(f1_findings[0].metric_value > 3.0);
    assert!(f1_findings[0].detail.contains("change entropy"));
}

#[test]
fn change_entropy_excludes_wide_commits() {
    let (_dir, db) = setup_db();
    let f1 = seed_file(&db, "src/target.rs");
    let mut extras = Vec::new();
    for i in 0..35 {
        extras.push(seed_file(&db, &format!("src/extra_{i}.rs")));
    }

    let now = 1_700_000_000i64;
    // One commit touching 36 files (> 30) — should be excluded
    let mut commits = vec![CommitRow {
        hash: "wide".into(),
        committed_at: now,
        author: "dev@x".into(),
    }];
    let mut pairs: Vec<(String, i64)> = vec![("wide".into(), f1)];
    for &eid in &extras {
        pairs.push(("wide".into(), eid));
    }
    // One narrow commit touching only f1 — F=1, log2(1)=0, no contribution
    commits.push(CommitRow {
        hash: "narrow".into(),
        committed_at: now,
        author: "dev@x".into(),
    });
    pairs.push(("narrow".into(), f1));

    seed_commits(&db, &commits, &pairs);

    let findings = compute_change_entropy(&db).unwrap();
    let f1_findings: Vec<_> = findings.iter().filter(|f| f.file_id == f1).collect();
    assert!(
        f1_findings.is_empty(),
        "wide commit excluded, single-file has zero entropy"
    );
}

#[test]
fn change_entropy_below_threshold() {
    let (_dir, db) = setup_db();
    let f1 = seed_file(&db, "src/calm.rs");
    let f2 = seed_file(&db, "src/calm2.rs");

    // 2 commits with F=2: each contributes ~0.5 → sum ≈ 1.0, below threshold 3.0
    let commits = vec![
        CommitRow {
            hash: "c1".into(),
            committed_at: 1_700_000_000,
            author: "a@b".into(),
        },
        CommitRow {
            hash: "c2".into(),
            committed_at: 1_700_000_000,
            author: "a@b".into(),
        },
    ];
    let pairs = vec![
        ("c1".into(), f1),
        ("c1".into(), f2),
        ("c2".into(), f1),
        ("c2".into(), f2),
    ];
    seed_commits(&db, &commits, &pairs);

    let findings = compute_change_entropy(&db).unwrap();
    let f1_findings: Vec<_> = findings.iter().filter(|f| f.file_id == f1).collect();
    assert!(
        f1_findings.is_empty(),
        "entropy ~1.0 is below threshold 3.0"
    );
}

#[test]
fn ownership_risk_top_owner_below_40() {
    let (dir, db) = setup_db();
    let fid = seed_file(&db, "src/shared.rs");

    // 3 authors with roughly equal commits: 35%, 35%, 30%
    let commits = vec![
        CommitRow {
            hash: "a1".into(),
            committed_at: 1_700_000_000,
            author: "alice@dev".into(),
        },
        CommitRow {
            hash: "a2".into(),
            committed_at: 1_700_000_001,
            author: "alice@dev".into(),
        },
        CommitRow {
            hash: "a3".into(),
            committed_at: 1_700_000_002,
            author: "alice@dev".into(),
        },
        CommitRow {
            hash: "a4".into(),
            committed_at: 1_700_000_003,
            author: "alice@dev".into(),
        },
        CommitRow {
            hash: "a5".into(),
            committed_at: 1_700_000_004,
            author: "alice@dev".into(),
        },
        CommitRow {
            hash: "a6".into(),
            committed_at: 1_700_000_005,
            author: "alice@dev".into(),
        },
        CommitRow {
            hash: "a7".into(),
            committed_at: 1_700_000_006,
            author: "alice@dev".into(),
        },
        CommitRow {
            hash: "b1".into(),
            committed_at: 1_700_000_007,
            author: "bob@dev".into(),
        },
        CommitRow {
            hash: "b2".into(),
            committed_at: 1_700_000_008,
            author: "bob@dev".into(),
        },
        CommitRow {
            hash: "b3".into(),
            committed_at: 1_700_000_009,
            author: "bob@dev".into(),
        },
        CommitRow {
            hash: "b4".into(),
            committed_at: 1_700_000_010,
            author: "bob@dev".into(),
        },
        CommitRow {
            hash: "b5".into(),
            committed_at: 1_700_000_011,
            author: "bob@dev".into(),
        },
        CommitRow {
            hash: "b6".into(),
            committed_at: 1_700_000_012,
            author: "bob@dev".into(),
        },
        CommitRow {
            hash: "b7".into(),
            committed_at: 1_700_000_013,
            author: "bob@dev".into(),
        },
        CommitRow {
            hash: "c1".into(),
            committed_at: 1_700_000_014,
            author: "carol@dev".into(),
        },
        CommitRow {
            hash: "c2".into(),
            committed_at: 1_700_000_015,
            author: "carol@dev".into(),
        },
        CommitRow {
            hash: "c3".into(),
            committed_at: 1_700_000_016,
            author: "carol@dev".into(),
        },
        CommitRow {
            hash: "c4".into(),
            committed_at: 1_700_000_017,
            author: "carol@dev".into(),
        },
        CommitRow {
            hash: "c5".into(),
            committed_at: 1_700_000_018,
            author: "carol@dev".into(),
        },
        CommitRow {
            hash: "c6".into(),
            committed_at: 1_700_000_019,
            author: "carol@dev".into(),
        },
    ];
    let pairs: Vec<(String, i64)> = commits.iter().map(|c| (c.hash.clone(), fid)).collect();
    seed_commits(&db, &commits, &pairs);

    let owners = sutra::health::probe::probe_owners(dir.path()).config;
    let findings = compute_ownership_risk(&db, &owners).unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].biomarker_kind, BiomarkerKind::OwnershipRisk);
    assert!(findings[0].detail.contains("top owner"));
    // max share = 7/20 = 35%
    assert!(findings[0].metric_value < 0.40);
}

#[test]
fn ownership_risk_minor_contributors() {
    let (dir, db) = setup_db();
    let fid = seed_file(&db, "src/many_hands.rs");

    // 1 major author (80%) + 4 minor (<5% each)
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..80 {
        let hash = format!("major_{i:03}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_000_000 + i,
            author: "major@dev".into(),
        });
        pairs.push((hash, fid));
    }
    for (j, minor) in ["m1@dev", "m2@dev", "m3@dev", "m4@dev"].iter().enumerate() {
        let hash = format!("minor_{j}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_100_000 + j as i64,
            author: minor.to_string(),
        });
        pairs.push((hash, fid));
    }
    seed_commits(&db, &commits, &pairs);

    let owners = sutra::health::probe::probe_owners(dir.path()).config;
    let findings = compute_ownership_risk(&db, &owners).unwrap();
    assert_eq!(findings.len(), 1);
    assert!(findings[0].detail.contains("minor contributors"));
}

#[test]
fn ownership_risk_with_alias_merging() {
    let (dir, db) = setup_db();
    let fid = seed_file(&db, "src/aliased.rs");

    // Create .sutra/owners.toml
    std::fs::create_dir_all(dir.path().join(".sutra")).unwrap();
    std::fs::write(
        dir.path().join(".sutra/owners.toml"),
        "[aliases]\n\"bot@ci\" = \"alice@dev\"\n",
    )
    .unwrap();

    // bot@ci (5 commits) + alice@dev (5 commits) → merged to alice@dev (10)
    // bob@dev (10 commits)
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..5 {
        let hash = format!("bot_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_000_000 + i,
            author: "bot@ci".into(),
        });
        pairs.push((hash, fid));
    }
    for i in 0..5 {
        let hash = format!("alice_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_000_100 + i,
            author: "alice@dev".into(),
        });
        pairs.push((hash, fid));
    }
    for i in 0..10 {
        let hash = format!("bob_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_000_200 + i,
            author: "bob@dev".into(),
        });
        pairs.push((hash, fid));
    }
    seed_commits(&db, &commits, &pairs);

    let owners = sutra::health::probe::probe_owners(dir.path()).config;
    let findings = compute_ownership_risk(&db, &owners).unwrap();
    // After aliasing: alice@dev = 10, bob@dev = 10 → 50% each, top owner = 50% >= 40%
    // Only 2 authors, no minor contributors → no finding should fire
    assert!(
        findings.is_empty(),
        "aliased authors merge; 50/50 split is healthy"
    );
}

#[test]
fn ownership_risk_no_alias_file_conservative() {
    let (dir, db) = setup_db();
    let fid = seed_file(&db, "src/no_alias.rs");

    // Without owners.toml: bot@ci and alice@dev are distinct
    // 3 authors each with ~33% → top < 40% → fires
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..5 {
        let hash = format!("bot_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_000_000 + i,
            author: "bot@ci".into(),
        });
        pairs.push((hash, fid));
    }
    for i in 0..5 {
        let hash = format!("alice_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_000_100 + i,
            author: "alice@dev".into(),
        });
        pairs.push((hash, fid));
    }
    for i in 0..5 {
        let hash = format!("bob_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_000_200 + i,
            author: "bob@dev".into(),
        });
        pairs.push((hash, fid));
    }
    seed_commits(&db, &commits, &pairs);

    let owners = sutra::health::probe::probe_owners(dir.path()).config;
    let findings = compute_ownership_risk(&db, &owners).unwrap();
    assert_eq!(findings.len(), 1, "3 authors at 33% each → top < 40%");
    assert!(findings[0].detail.contains("top owner"));
}

#[test]
fn hidden_coupling_fires_without_static_edge() {
    let (_dir, db) = setup_db();
    let fa = seed_file(&db, "src/alpha.rs");
    let fb = seed_file(&db, "src/beta.rs");

    // 10 shared commits, 0 individual → Jaccard = 1.0
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..10 {
        let hash = format!("hc_{i:02}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_000_000 + i,
            author: "dev@x".into(),
        });
        pairs.push((hash.clone(), fa));
        pairs.push((hash, fb));
    }
    seed_commits(&db, &commits, &pairs);

    let findings = compute_hidden_coupling(&db).unwrap();
    let relevant: Vec<_> = findings
        .iter()
        .filter(|f| f.file_id == fa || f.file_id == fb)
        .collect();
    assert_eq!(relevant.len(), 2, "one finding per file in the pair");
    assert_eq!(relevant[0].biomarker_kind, BiomarkerKind::HiddenCoupling);
    assert!(relevant[0].detail.contains("hidden coupling"));
    assert!(relevant[0].metric_value >= 0.50);
}

#[test]
fn hidden_coupling_suppressed_by_static_edge() {
    let (_dir, db) = setup_db();
    let fa = seed_file(&db, "src/importer.rs");
    let fb = seed_file(&db, "src/imported.rs");

    // Create a static ref from fa to a symbol in fb
    let sym_id = seed_fn(&db, fb, "imported::helper", "helper", None);
    db.insert_ref(fa, Some(sym_id), Some("helper"), 5, 0, "use")
        .unwrap();

    // High co-change
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..10 {
        let hash = format!("sr_{i:02}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_000_000 + i,
            author: "dev@x".into(),
        });
        pairs.push((hash.clone(), fa));
        pairs.push((hash, fb));
    }
    seed_commits(&db, &commits, &pairs);

    let findings = compute_hidden_coupling(&db).unwrap();
    let relevant: Vec<_> = findings
        .iter()
        .filter(|f| f.file_id == fa || f.file_id == fb)
        .collect();
    assert!(
        relevant.is_empty(),
        "static edge suppresses hidden coupling"
    );
}

#[test]
fn hidden_coupling_severity_escalation() {
    let (_dir, db) = setup_db();
    let f_low = seed_file(&db, "src/low.rs");
    let f_low_partner = seed_file(&db, "src/low_partner.rs");
    let f_high = seed_file(&db, "src/high.rs");
    let f_high_partner = seed_file(&db, "src/high_partner.rs");

    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    // f_low + f_low_partner: 6 shared, 3 only-low, 2 only-partner
    // jaccard = 6 / (6+3+2) = 6/11 ≈ 0.545 → Informational
    for i in 0..6 {
        let hash = format!("sl_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_000_000 + i,
            author: "dev@x".into(),
        });
        pairs.push((hash.clone(), f_low));
        pairs.push((hash, f_low_partner));
    }
    for i in 0..3 {
        let hash = format!("ol_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_100_000 + i,
            author: "dev@x".into(),
        });
        pairs.push((hash, f_low));
    }
    for i in 0..2 {
        let hash = format!("olp_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_100_100 + i,
            author: "dev@x".into(),
        });
        pairs.push((hash, f_low_partner));
    }
    // f_high + f_high_partner: 10 shared, 2 only-high, 1 only-partner
    // jaccard = 10 / (10+2+1) = 10/13 ≈ 0.769 → Advisory
    for i in 0..10 {
        let hash = format!("sh_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_200_000 + i,
            author: "dev@x".into(),
        });
        pairs.push((hash.clone(), f_high));
        pairs.push((hash, f_high_partner));
    }
    for i in 0..2 {
        let hash = format!("oh_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: 1_700_300_000 + i,
            author: "dev@x".into(),
        });
        pairs.push((hash, f_high));
    }
    let hash = "ohp_0".to_string();
    commits.push(CommitRow {
        hash: hash.clone(),
        committed_at: 1_700_300_100,
        author: "dev@x".into(),
    });
    pairs.push((hash, f_high_partner));

    seed_commits(&db, &commits, &pairs);

    let findings = compute_hidden_coupling(&db).unwrap();
    let low_finding = findings.iter().find(|f| f.file_id == f_low).unwrap();
    let high_finding = findings.iter().find(|f| f.file_id == f_high).unwrap();
    assert_eq!(low_finding.severity, HealthSeverity::Informational);
    assert_eq!(high_finding.severity, HealthSeverity::Advisory);
}

// --- dead_code_ratio biomarker ---

fn seed_priv_fn(db: &Db, file_id: i64, qn: &str, sn: &str) -> i64 {
    db.insert_symbol(&InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind: "function",
        signature: None,
        signature_hash: None,
        structural_hash: None,
        visibility: None,
        start_line: 1,
        start_col: 0,
        end_line: 10,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: Some(1),
        cognitive: Some(0),
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    })
    .unwrap()
}

#[test]
fn dead_code_ratio_fires_above_threshold() {
    let (_dir, db) = setup_db();
    let file = seed_file(&db, "src/dead.rs");
    // 4 symbols, 3 unreferenced → ratio 0.75, well above 0.15.
    let used = seed_priv_fn(&db, file, "used", "used");
    seed_priv_fn(&db, file, "dead_a", "dead_a");
    seed_priv_fn(&db, file, "dead_b", "dead_b");
    seed_priv_fn(&db, file, "dead_c", "dead_c");
    // A ref targeting `used` keeps it alive → dead 3/4 = 0.75.
    db.insert_ref(file, Some(used), None, 5, 0, "call").unwrap();

    let findings = sutra::health::findings::compute_dead_code_ratio(&db).unwrap();
    let f = findings.iter().find(|f| f.file_id == file).unwrap();
    assert_eq!(f.biomarker_kind, BiomarkerKind::DeadCodeRatio);
    assert_eq!(f.symbol_id, None);
    assert!(
        f.metric_value >= 0.15,
        "ratio {} below threshold",
        f.metric_value
    );
    assert!(f.detail.contains("unreferenced"));
}

#[test]
fn dead_code_ratio_silent_when_all_referenced() {
    let (_dir, db) = setup_db();
    let file = seed_file(&db, "src/live.rs");
    let a = seed_priv_fn(&db, file, "a", "a");
    let b = seed_priv_fn(&db, file, "b", "b");
    // a → b, b → a: both referenced.
    db.insert_ref(file, Some(b), None, 2, 0, "call").unwrap();
    db.insert_ref(file, Some(a), None, 3, 0, "call").unwrap();

    let findings = sutra::health::findings::compute_dead_code_ratio(&db).unwrap();
    assert!(findings.iter().all(|f| f.file_id != file));
}

// --- blast_radius_churn biomarker ---

#[test]
fn blast_radius_churn_fires_when_hub_churns() {
    let (_dir, db) = setup_db();
    let hub = seed_file(&db, "src/hub.rs");
    let quiet = seed_file(&db, "src/quiet.rs");
    // hub: wide blast radius, many commits; quiet: wide blast radius, no churn.
    db.update_rollups(hub, 3, 20).unwrap();
    db.update_rollups(quiet, 3, 20).unwrap();

    let now = 1_700_000_000i64;
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..6 {
        let hash = format!("bc_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: now + i * 86400,
            author: "dev@x".into(),
        });
        pairs.push((hash, hub));
    }
    seed_commits(&db, &commits, &pairs);

    let findings = compute_blast_radius_churn(&db).unwrap();
    let hub_f = findings.iter().find(|f| f.file_id == hub).unwrap();
    assert_eq!(hub_f.biomarker_kind, BiomarkerKind::BlastRadiusChurn);
    assert_eq!(hub_f.severity, HealthSeverity::Advisory);
    assert!(hub_f.metric_value >= 10.0);
    assert!(
        findings.iter().all(|f| f.file_id != quiet),
        "quiet file has no churn, should not fire"
    );
}

#[test]
fn blast_radius_churn_silent_when_narrow() {
    let (_dir, db) = setup_db();
    let leaf = seed_file(&db, "src/leaf.rs");
    // Churns a lot but nothing depends on it.
    db.update_rollups(leaf, 0, 1).unwrap();
    let now = 1_700_000_000i64;
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..8 {
        let hash = format!("lf_{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: now + i * 86400,
            author: "dev@x".into(),
        });
        pairs.push((hash, leaf));
    }
    seed_commits(&db, &commits, &pairs);

    let findings = compute_blast_radius_churn(&db).unwrap();
    assert!(findings.iter().all(|f| f.file_id != leaf));
}

// --- Snapshot storage ---

fn insert_snapshot(db: &Db, health_score: f64) -> i64 {
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 10,
        symbols_extracted: 50,
        refs_extracted: 30,
        parse_errors: 0,
        duration_ms: 100,
        total_complexity: 20,
        dead_symbol_count: 2,
        hotspot_count: 1,
        health_score,
        pattern_family_count: 3,
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn test_snapshot_stores_per_file_health() {
    let (_dir, db) = setup_db();
    let snap_id = insert_snapshot(&db, 8.5);

    let files = vec![
        SnapshotFileRow {
            file_id: 1,
            file_path: "src/foo.rs".into(),
            score: 9.2,
            category_scores: r#"{"structural":0.8}"#.into(),
            completeness: SnapshotCompleteness::Complete,
            missing_biomarkers: Vec::new(),
            score_upper: None,
            score_basis: Some("basis-v1".into()),
        },
        SnapshotFileRow {
            file_id: 2,
            file_path: "src/bar.rs".into(),
            score: 6.1,
            category_scores: r#"{"organizational":2.5,"structural":1.4}"#.into(),
            completeness: SnapshotCompleteness::Complete,
            missing_biomarkers: Vec::new(),
            score_upper: None,
            score_basis: Some("basis-v1".into()),
        },
    ];
    db.insert_snapshot_files(snap_id, &files).unwrap();

    let loaded = db.snapshot_file_scores(snap_id).unwrap();
    assert_eq!(loaded.len(), 2);

    let foo = loaded.iter().find(|f| f.file_path == "src/foo.rs").unwrap();
    assert!((foo.score - 9.2).abs() < 0.01);
    assert!(foo.category_scores.contains("structural"));

    let bar = loaded.iter().find(|f| f.file_path == "src/bar.rs").unwrap();
    assert!((bar.score - 6.1).abs() < 0.01);
    assert!(bar.category_scores.contains("organizational"));
}

#[test]
fn test_snapshot_stores_per_component_health() {
    let (_dir, db) = setup_db();
    let snap_id = insert_snapshot(&db, 7.8);

    let comps = vec![
        SnapshotComponentRow {
            component_id: "comp_a".into(),
            component_name: "auth".into(),
            score: 8.3,
            member_count: 5,
            total_nloc: 1200,
            completeness: SnapshotCompleteness::Complete,
            score_basis: Some("comp-basis-v1".into()),
            members: Some(vec![
                SnapshotComponentMember {
                    file_path: "src/auth/a.rs".into(),
                    weight: 700,
                },
                SnapshotComponentMember {
                    file_path: "src/auth/b.rs".into(),
                    weight: 500,
                },
            ]),
            instability_penalty: Some(0.25),
        },
        SnapshotComponentRow {
            component_id: "comp_b".into(),
            component_name: "db".into(),
            score: 6.9,
            member_count: 3,
            total_nloc: 800,
            completeness: SnapshotCompleteness::Complete,
            score_basis: Some("comp-basis-v1".into()),
            members: None,
            instability_penalty: None,
        },
    ];
    db.insert_snapshot_components(snap_id, &comps).unwrap();

    let loaded = db.snapshot_component_scores(snap_id).unwrap();
    assert_eq!(loaded.len(), 2);

    let auth = loaded.iter().find(|c| c.component_id == "comp_a").unwrap();
    assert!((auth.score - 8.3).abs() < 0.01);
    assert_eq!(auth.member_count, 5);
    assert_eq!(auth.total_nloc, 1200);
    let mut members: Vec<(&str, i64)> = auth
        .members
        .as_deref()
        .expect("weights recorded")
        .iter()
        .map(|m| (m.file_path.as_str(), m.weight))
        .collect();
    members.sort_unstable();
    assert_eq!(members, [("src/auth/a.rs", 700), ("src/auth/b.rs", 500)]);
    assert_eq!(auth.instability_penalty, Some(0.25));

    let db_comp = loaded.iter().find(|c| c.component_id == "comp_b").unwrap();
    assert!((db_comp.score - 6.9).abs() < 0.01);
    assert!(
        db_comp.members.is_none(),
        "unrecorded weights read back as unknown, not an empty member list"
    );
    assert_eq!(db_comp.instability_penalty, None);
}

#[test]
fn test_file_health_history() {
    let (_dir, db) = setup_db();

    let snap1 = insert_snapshot(&db, 7.0);
    db.insert_snapshot_files(
        snap1,
        &[SnapshotFileRow {
            file_id: 1,
            file_path: "src/main.rs".into(),
            score: 7.5,
            category_scores: r#"{"structural":1.0}"#.into(),
            completeness: SnapshotCompleteness::Complete,
            missing_biomarkers: Vec::new(),
            score_upper: None,
            score_basis: Some("basis-v1".into()),
        }],
    )
    .unwrap();

    let snap2 = insert_snapshot(&db, 8.0);
    db.insert_snapshot_files(
        snap2,
        &[SnapshotFileRow {
            file_id: 1,
            file_path: "src/main.rs".into(),
            score: 8.2,
            category_scores: r#"{"structural":0.5}"#.into(),
            completeness: SnapshotCompleteness::Complete,
            missing_biomarkers: Vec::new(),
            score_upper: None,
            score_basis: Some("basis-v1".into()),
        }],
    )
    .unwrap();

    let snap3 = insert_snapshot(&db, 9.0);
    db.insert_snapshot_files(
        snap3,
        &[SnapshotFileRow {
            file_id: 1,
            file_path: "src/main.rs".into(),
            score: 9.1,
            category_scores: "{}".into(),
            completeness: SnapshotCompleteness::Complete,
            missing_biomarkers: Vec::new(),
            score_upper: None,
            score_basis: Some("basis-v1".into()),
        }],
    )
    .unwrap();

    let history = db.file_health_history("src/main.rs", 10).unwrap();
    assert_eq!(history.len(), 3);
    // Newest first
    assert!((history[0].score - 9.1).abs() < 0.01);
    assert!((history[1].score - 8.2).abs() < 0.01);
    assert!((history[2].score - 7.5).abs() < 0.01);

    // Limit works
    let limited = db.file_health_history("src/main.rs", 2).unwrap();
    assert_eq!(limited.len(), 2);

    // Non-existent file returns empty
    let empty = db.file_health_history("src/nope.rs", 10).unwrap();
    assert!(empty.is_empty());
}

#[test]
fn test_snapshot_pattern_family_count_roundtrip() {
    let (_dir, db) = setup_db();
    insert_snapshot(&db, 8.0);

    let snaps = db.latest_snapshots(1).unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].pattern_family_count, 3);
    assert!((snaps[0].health_score - 8.0).abs() < 0.01);
}

#[test]
fn test_trend_comparison_with_file_deltas() {
    let (_dir, db) = setup_db();

    let snap1 = insert_snapshot(&db, 7.0);
    db.insert_snapshot_files(
        snap1,
        &[
            SnapshotFileRow {
                file_id: 1,
                file_path: "src/a.rs".into(),
                score: 8.0,
                category_scores: r#"{"structural":1.0}"#.into(),
                completeness: SnapshotCompleteness::Complete,
                missing_biomarkers: Vec::new(),
                score_upper: None,
                score_basis: Some("basis-v1".into()),
            },
            SnapshotFileRow {
                file_id: 2,
                file_path: "src/b.rs".into(),
                score: 6.0,
                category_scores: r#"{"organizational":2.0}"#.into(),
                completeness: SnapshotCompleteness::Complete,
                missing_biomarkers: Vec::new(),
                score_upper: None,
                score_basis: Some("basis-v1".into()),
            },
        ],
    )
    .unwrap();

    let snap2 = insert_snapshot(&db, 8.0);
    db.insert_snapshot_files(
        snap2,
        &[
            SnapshotFileRow {
                file_id: 1,
                file_path: "src/a.rs".into(),
                score: 9.0,
                category_scores: r#"{"structural":0.5}"#.into(),
                completeness: SnapshotCompleteness::Complete,
                missing_biomarkers: Vec::new(),
                score_upper: None,
                score_basis: Some("basis-v1".into()),
            },
            SnapshotFileRow {
                file_id: 2,
                file_path: "src/b.rs".into(),
                score: 5.0,
                category_scores: r#"{"organizational":3.0}"#.into(),
                completeness: SnapshotCompleteness::Complete,
                missing_biomarkers: Vec::new(),
                score_upper: None,
                score_basis: Some("basis-v1".into()),
            },
        ],
    )
    .unwrap();

    let args = sutra::tools::trend::TrendArgs {
        workspace: String::new(),
        from: None,
        to: None,
        path: None,
        limit: None,
    };
    let result = sutra::tools::trend::handle(&db, &args).unwrap();

    // Aggregate health delta: complete evidence, same population → measured.
    assert_eq!(result["aggregate_comparison"]["measured"], true);
    assert!(result["aggregate_comparison"]["reason"].is_null());
    let deltas = &result["deltas"];
    assert!((deltas["health_score"].as_f64().unwrap() - 1.0).abs() < 0.01);
    assert_eq!(deltas["pattern_family_count"].as_i64().unwrap(), 0);

    // Per-file deltas
    let improved = result["files"]["improved"].as_array().unwrap();
    assert_eq!(improved.len(), 1);
    assert_eq!(improved[0]["path"].as_str().unwrap(), "src/a.rs");
    assert!((improved[0]["delta"].as_f64().unwrap() - 1.0).abs() < 0.01);

    let degraded = result["files"]["degraded"].as_array().unwrap();
    assert_eq!(degraded.len(), 1);
    assert_eq!(degraded[0]["path"].as_str().unwrap(), "src/b.rs");
    assert!((degraded[0]["delta"].as_f64().unwrap() - (-1.0)).abs() < 0.01);

    // Category deltas
    let cats = &result["categories"];
    let structural = &cats["structural"];
    assert!((structural["from"].as_f64().unwrap() - 1.0).abs() < 0.01);
    assert!((structural["to"].as_f64().unwrap() - 0.5).abs() < 0.01);
}

#[test]
fn test_trend_file_history_mode() {
    let (_dir, db) = setup_db();

    let snap1 = insert_snapshot(&db, 7.0);
    db.insert_snapshot_files(
        snap1,
        &[SnapshotFileRow {
            file_id: 1,
            file_path: "src/x.rs".into(),
            score: 7.0,
            category_scores: "{}".into(),
            completeness: SnapshotCompleteness::Complete,
            missing_biomarkers: Vec::new(),
            score_upper: None,
            score_basis: Some("basis-v1".into()),
        }],
    )
    .unwrap();

    let snap2 = insert_snapshot(&db, 9.0);
    db.insert_snapshot_files(
        snap2,
        &[SnapshotFileRow {
            file_id: 1,
            file_path: "src/x.rs".into(),
            score: 9.5,
            category_scores: "{}".into(),
            completeness: SnapshotCompleteness::Complete,
            missing_biomarkers: Vec::new(),
            score_upper: None,
            score_basis: Some("basis-v1".into()),
        }],
    )
    .unwrap();

    let args = sutra::tools::trend::TrendArgs {
        workspace: String::new(),
        from: None,
        to: None,
        path: Some("src/x.rs".into()),
        limit: None,
    };
    let result = sutra::tools::trend::handle(&db, &args).unwrap();

    assert_eq!(result["mode"].as_str().unwrap(), "history");
    assert_eq!(result["path"].as_str().unwrap(), "src/x.rs");
    let snapshots = result["snapshots"].as_array().unwrap();
    assert_eq!(snapshots.len(), 2);
    assert!((snapshots[0]["health_score"].as_f64().unwrap() - 9.5).abs() < 0.01);
    assert!((snapshots[1]["health_score"].as_f64().unwrap() - 7.0).abs() < 0.01);
}

/// sutra/438: a partial row's stored score is only its lower bound and a legacy
/// row's was scored under other rules — neither is a numeric health_score.
#[test]
fn test_trend_history_partial_and_legacy_rows_are_not_measured() {
    let (_dir, db) = setup_db();

    let legacy = insert_snapshot(&db, 6.0);
    let mut row = snap_row(1, "src/x.rs", 6.0, SnapshotCompleteness::Unknown, &[]);
    row.score_basis = None;
    db.insert_snapshot_files(legacy, &[row]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));

    let no_basis = insert_snapshot(&db, 6.5);
    let mut row = snap_row(1, "src/x.rs", 6.5, SnapshotCompleteness::Complete, &[]);
    row.score_basis = None;
    db.insert_snapshot_files(no_basis, &[row]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));

    let partial = insert_snapshot(&db, 7.0);
    let mut row = snap_row(
        1,
        "src/x.rs",
        4.0,
        SnapshotCompleteness::Partial,
        &["change_entropy"],
    );
    row.score_upper = Some(8.5);
    db.insert_snapshot_files(partial, &[row]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));

    let complete = insert_snapshot(&db, 9.0);
    let row = snap_row(1, "src/x.rs", 9.25, SnapshotCompleteness::Complete, &[]);
    db.insert_snapshot_files(complete, &[row]).unwrap();

    let result = trend(&db, Some("src/x.rs"));
    let snaps = result["snapshots"].as_array().unwrap();
    assert_eq!(snaps.len(), 4);

    // Newest first.
    assert_eq!(snaps[0]["completeness"], "complete");
    assert!((snaps[0]["health_score"].as_f64().unwrap() - 9.25).abs() < 0.001);
    assert!(snaps[0].get("score_bounds").is_none());

    assert_eq!(snaps[1]["completeness"], "partial");
    assert!(snaps[1]["health_score"].is_null());
    assert!((snaps[1]["score_bounds"]["lower"].as_f64().unwrap() - 4.0).abs() < 0.001);
    assert!((snaps[1]["score_bounds"]["upper"].as_f64().unwrap() - 8.5).abs() < 0.001);
    assert_eq!(snaps[1]["partial"], true);

    for legacy in &snaps[2..] {
        assert!(legacy["health_score"].is_null(), "legacy row: {legacy}");
        assert!(legacy.get("score_bounds").is_none());
        assert!(legacy["legacy_score"].is_number());
    }
    assert_eq!(snaps[3]["completeness"], "unknown");
}

#[test]
fn test_trend_history_corrupt_category_scores_is_an_error() {
    let (_dir, db) = setup_db();
    let snap = insert_snapshot(&db, 7.0);
    let mut row = snap_row(1, "src/x.rs", 7.0, SnapshotCompleteness::Complete, &[]);
    row.category_scores = "{not json".into();
    db.insert_snapshot_files(snap, &[row]).unwrap();

    let args = sutra::tools::trend::TrendArgs {
        workspace: String::new(),
        from: None,
        to: None,
        path: Some("src/x.rs".into()),
        limit: None,
    };
    let err = sutra::tools::trend::handle(&db, &args).unwrap_err();
    assert!(err.to_string().contains("category_scores"), "{err}");
}

/// sutra/441: comparison mode must not drop a corrupt row from the category
/// totals — that would read lost data as a debt change.
#[test]
fn test_trend_comparison_corrupt_category_scores_is_an_error() {
    let (_dir, db) = setup_db();
    let from = insert_snapshot(&db, 7.0);
    let mut row = snap_row(1, "src/x.rs", 7.0, SnapshotCompleteness::Complete, &[]);
    row.category_scores = "{not json".into();
    db.insert_snapshot_files(from, &[row]).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let to = insert_snapshot(&db, 8.0);
    let row = snap_row(1, "src/x.rs", 8.0, SnapshotCompleteness::Complete, &[]);
    db.insert_snapshot_files(to, &[row]).unwrap();

    let args = sutra::tools::trend::TrendArgs {
        workspace: String::new(),
        from: None,
        to: None,
        path: None,
        limit: None,
    };
    let err = sutra::tools::trend::handle(&db, &args).unwrap_err();
    assert!(err.to_string().contains("category_scores"), "{err}");
    assert!(err.to_string().contains("src/x.rs"), "{err}");
}

// --- Blame parsing ---

#[test]
fn test_parse_blame_porcelain_basic() {
    let input = "\
aabbccdd11223344556677889900aabbccddeeff 1 1 2
author Alice
author-mail <alice@dev>
author-time 1700000000
author-tz +0000
committer Alice
committer-mail <alice@dev>
committer-time 1700000000
committer-tz +0000
summary initial commit
filename src/main.rs
\tfn main() {
aabbccdd11223344556677889900aabbccddeeff 2 2
\t    println!(\"hello\");
ff00112233445566778899aabbccddeeff001122 3 3 1
author Bob
author-mail <bob@dev>
author-time 1700100000
author-tz +0000
committer Bob
committer-mail <bob@dev>
committer-time 1700100000
committer-tz +0000
summary add closing brace
filename src/main.rs
\t}
";
    let lines = parse_blame_porcelain(input);
    assert_eq!(lines.len(), 3);

    assert_eq!(lines[0].commit, "aabbccdd11223344556677889900aabbccddeeff");
    assert_eq!(lines[0].author_time, 1700000000);
    assert_eq!(lines[0].line_no, 1);

    assert_eq!(lines[1].commit, "aabbccdd11223344556677889900aabbccddeeff");
    assert_eq!(lines[1].author_time, 1700000000);
    assert_eq!(lines[1].line_no, 2);

    assert_eq!(lines[2].commit, "ff00112233445566778899aabbccddeeff001122");
    assert_eq!(lines[2].author_time, 1700100000);
    assert_eq!(lines[2].line_no, 3);
}

#[test]
fn test_parse_blame_porcelain_interleaved_repeat() {
    // Commit A appears, then B, then A again without metadata.
    // The parser must use A's cached timestamp, not B's.
    let input = "\
aabbccdd11223344556677889900aabbccddeeff 1 1 1
author Alice
author-mail <alice@dev>
author-time 1700000000
author-tz +0000
committer Alice
committer-mail <alice@dev>
committer-time 1700000000
committer-tz +0000
summary first
filename src/main.rs
\tline one
ff00112233445566778899aabbccddeeff001122 2 2 1
author Bob
author-mail <bob@dev>
author-time 1700100000
author-tz +0000
committer Bob
committer-mail <bob@dev>
committer-time 1700100000
committer-tz +0000
summary second
filename src/main.rs
\tline two
aabbccdd11223344556677889900aabbccddeeff 3 3
\tline three
";
    let lines = parse_blame_porcelain(input);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0].author_time, 1700000000);
    assert_eq!(lines[1].author_time, 1700100000);
    // Line 3 is commit A again — must have A's timestamp, not B's
    assert_eq!(lines[2].commit, "aabbccdd11223344556677889900aabbccddeeff");
    assert_eq!(lines[2].author_time, 1700000000);
}

#[test]
fn test_parse_blame_porcelain_empty() {
    let lines = parse_blame_porcelain("");
    assert!(lines.is_empty());
}

// --- HealthFinding::to_row ---

#[test]
fn test_health_finding_to_row() {
    let finding = HealthFinding {
        file_id: 42,
        symbol_id: Some(7),
        biomarker_kind: BiomarkerKind::FunctionHotspot,
        severity: HealthSeverity::Advisory,
        confidence: 1.0,
        provenance: "on-demand:blame".into(),
        metric_value: 15.0,
        threshold: 8.0,
        detail: "test detail".into(),
    };
    let row = finding.to_row(-1);
    assert_eq!(row.id, -1);
    assert_eq!(row.file_id, 42);
    assert_eq!(row.symbol_id, Some(7));
    assert_eq!(row.biomarker_kind, "function_hotspot");
    assert_eq!(row.severity, "advisory");
    assert_eq!(row.metric_value, 15.0);
}

// --- Validated persistent evidence (sutra/416) ---

fn nested_finding(file_id: i64) -> HealthFinding {
    HealthFinding {
        file_id,
        symbol_id: None,
        biomarker_kind: BiomarkerKind::NestedComplexity,
        severity: HealthSeverity::Advisory,
        confidence: 1.0,
        provenance: "computed".into(),
        metric_value: 6.0,
        threshold: 4.0,
        detail: "deep".into(),
    }
}

#[test]
fn evidence_scores_the_current_run_as_measured() {
    use sutra::health::assess::PersistentEvidence;
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/hot.rs");
    db.replace_health_findings(&[nested_finding(fid)]).unwrap();
    publish_seeded_run(&db);

    let ev = PersistentEvidence::load(&db, health_run::current_verdict(&db)).unwrap();
    let s = ev.file("src/hot.rs").unwrap().score();
    let expected = 10.0 - sat(HealthCategory::Structural, 1.34);
    assert!(matches!(s.value, ScoreValue::Measured(v) if (v - expected).abs() < 1e-9));
}

#[test]
fn evidence_from_a_stale_run_never_counts_its_findings_as_current_debt() {
    use sutra::health::assess::PersistentEvidence;
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/hot.rs");
    db.replace_health_findings(&[nested_finding(fid)]).unwrap();
    publish_seeded_run(&db);

    let ev = PersistentEvidence::load(
        &db,
        sutra::health::assess::RunVerdict::stale(MissingReason::InputsChanged),
    )
    .unwrap();
    let f = ev.file("src/hot.rs").unwrap();
    assert_eq!(f.findings.len(), 1, "stale findings stay visible");
    let s = f.score();
    assert!(s.deductions.is_empty(), "…but are not current debt");
    assert_eq!(s.value.upper(), 10.0);
    assert!(s.partial());
    assert!(
        s.missing
            .iter()
            .all(|m| m.reason == MissingReason::InputsChanged)
    );
}

#[test]
fn evidence_without_any_run_is_legacy_partial_not_clean() {
    use sutra::health::assess::PersistentEvidence;
    let (_dir, db) = setup_db();
    seed_file(&db, "src/a.rs");
    db.replace_health_findings(&[]).unwrap();
    let ev = PersistentEvidence::load(&db, health_run::current_verdict(&db)).unwrap();
    let s = ev.file("src/a.rs").unwrap().score();
    assert!(s.partial());
    assert!(
        s.missing
            .iter()
            .all(|m| m.reason == MissingReason::LegacyUnknown)
    );
}

#[test]
fn evidence_marks_a_file_added_after_the_run_never_computed() {
    use sutra::health::assess::PersistentEvidence;
    let (_dir, db) = setup_db();
    seed_file(&db, "src/a.rs");
    db.replace_health_findings(&[]).unwrap();
    publish_seeded_run(&db);
    seed_file(&db, "src/new.rs");
    // Validity is the caller's claim; even a (wrongly) current claim cannot make
    // a file the run never saw complete.
    let ev = PersistentEvidence::load(&db, health_run::current_verdict(&db)).unwrap();
    assert!(ev.file("src/a.rs").unwrap().score().value.is_measured());
    let new = ev.file("src/new.rs").unwrap().score();
    assert!(new.partial());
    assert!(
        new.missing
            .iter()
            .all(|m| m.reason == MissingReason::NeverComputed)
    );
}

#[test]
fn evidence_applies_waivers_and_records_them_in_the_basis() {
    use sutra::health::assess::PersistentEvidence;
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/hot.rs");
    db.replace_health_findings(&[nested_finding(fid)]).unwrap();
    publish_seeded_run(&db);
    let before = PersistentEvidence::load(&db, health_run::current_verdict(&db)).unwrap();
    let before_basis = before.file("src/hot.rs").unwrap().basis;

    db.create_health_waiver("nested_complexity", "src/hot.rs", None, "accepted", "test")
        .unwrap();
    let after = PersistentEvidence::load(&db, health_run::current_verdict(&db)).unwrap();
    let f = after.file("src/hot.rs").unwrap();
    assert!(f.findings.is_empty());
    assert_eq!(f.waived.len(), 1);
    assert_eq!(f.score().value, ScoreValue::Measured(10.0));
    assert_ne!(
        f.basis, before_basis,
        "a waiver changes the scoring basis, so the 8.66 → 10.0 move is not a measured improvement"
    );
}

#[test]
fn baseline_selector_pinned_missing_stays_missing() {
    let (_dir, db) = setup_db();
    insert_snapshot(&db, 9.0);
    assert_eq!(BaselineSelector::Pinned(None).resolve(&db).unwrap(), None);
    let pinned = insert_snapshot(&db, 8.0);
    let latest = insert_snapshot(&db, 7.0);
    assert_eq!(
        BaselineSelector::Pinned(Some(pinned)).resolve(&db).unwrap(),
        Some(pinned)
    );
    assert_eq!(BaselineSelector::Latest.resolve(&db).unwrap(), Some(latest));
}

// ---------------------------------------------------------------------------
// Component instability (Martin's Ce/(Ca+Ce))
// ---------------------------------------------------------------------------

#[test]
fn component_instability_basic() {
    let (_dir, db) = setup_db();

    let fa = seed_file(&db, "src/alpha/a.rs");
    let fb = seed_file(&db, "src/beta/b.rs");
    let fc = seed_file(&db, "src/alpha/c.rs");

    db.insert_component("alpha", "Alpha").unwrap();
    db.insert_component("beta", "Beta").unwrap();
    db.batch_insert_membership(&[
        ("alpha".into(), fa),
        ("alpha".into(), fc),
        ("beta".into(), fb),
    ])
    .unwrap();

    // alpha imports beta (2 edges out), beta imports alpha (1 edge out)
    db.insert_import(fa, "src/beta/b.rs", Some(fb), 1, "use", None)
        .unwrap();
    db.insert_import(fc, "src/beta/b.rs", Some(fb), 1, "use", None)
        .unwrap();
    db.insert_import(fb, "src/alpha/a.rs", Some(fa), 1, "use", None)
        .unwrap();

    let result = compute_component_instability(&db).unwrap();

    // Alpha: Ce=2 (a→b, c→b), Ca=1 (b→a). I = 2/3
    let alpha = result.get("alpha").unwrap();
    assert_eq!(alpha.ce, 2);
    assert_eq!(alpha.ca, 1);
    assert!((alpha.instability - 2.0 / 3.0).abs() < 1e-9);

    // Beta: Ce=1 (b→a), Ca=2 (a→b, c→b). I = 1/3
    let beta = result.get("beta").unwrap();
    assert_eq!(beta.ce, 1);
    assert_eq!(beta.ca, 2);
    assert!((beta.instability - 1.0 / 3.0).abs() < 1e-9);
}

#[test]
fn component_instability_isolated() {
    let (_dir, db) = setup_db();

    let fa = seed_file(&db, "src/solo/a.rs");
    let fb = seed_file(&db, "src/solo/b.rs");

    db.insert_component("solo", "Solo").unwrap();
    db.batch_insert_membership(&[("solo".into(), fa), ("solo".into(), fb)])
        .unwrap();

    // Internal edge only — same component
    db.insert_import(fa, "src/solo/b.rs", Some(fb), 1, "use", None)
        .unwrap();

    let result = compute_component_instability(&db).unwrap();
    let solo = result.get("solo").unwrap();
    assert_eq!(solo.ce, 0);
    assert_eq!(solo.ca, 0);
    assert!((solo.instability - 0.0).abs() < 1e-9);
}

#[test]
fn component_instability_fully_efferent() {
    let (_dir, db) = setup_db();

    let fa = seed_file(&db, "src/leaf/a.rs");
    let fb = seed_file(&db, "src/core/b.rs");

    db.insert_component("leaf", "Leaf").unwrap();
    db.insert_component("core", "Core").unwrap();
    db.batch_insert_membership(&[("leaf".into(), fa), ("core".into(), fb)])
        .unwrap();

    // leaf → core only
    db.insert_import(fa, "src/core/b.rs", Some(fb), 1, "use", None)
        .unwrap();

    let result = compute_component_instability(&db).unwrap();

    let leaf = result.get("leaf").unwrap();
    assert_eq!(leaf.ce, 1);
    assert_eq!(leaf.ca, 0);
    assert!((leaf.instability - 1.0).abs() < 1e-9);

    let core = result.get("core").unwrap();
    assert_eq!(core.ce, 0);
    assert_eq!(core.ca, 1);
    assert!((core.instability - 0.0).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// File health: component filter + instability
// ---------------------------------------------------------------------------

#[test]
fn file_health_component_filter() {
    let (_dir, db) = setup_db();

    let fa = seed_file(&db, "src/alpha/a.rs");
    let fb = seed_file(&db, "src/beta/b.rs");

    seed_fn(&db, fa, "alpha::deep", "deep", Some(6));
    seed_fn(&db, fb, "beta::deep", "deep", Some(7));

    db.insert_component("alpha", "Alpha").unwrap();
    db.insert_component("beta", "Beta").unwrap();
    db.batch_insert_membership(&[("alpha".into(), fa), ("beta".into(), fb)])
        .unwrap();

    let findings = compute_nested_complexity(&db).unwrap();
    db.replace_health_findings(&findings).unwrap();
    publish_seeded_run(&db);

    // Without filter: both files + component summary
    let all = sutra::tools::file_health::handle(
        &db,
        health_run::current_verdict(&db),
        None,
        None,
        None,
        None,
        false,
    )
    .unwrap();
    assert_eq!(all["total_files"].as_u64().unwrap(), 2);
    assert!(
        all.get("components").is_some(),
        "unfiltered should include component scores"
    );

    // With component filter: only Alpha's file, no component summary
    let filtered = sutra::tools::file_health::handle(
        &db,
        health_run::current_verdict(&db),
        None,
        None,
        None,
        Some("Alpha"),
        false,
    )
    .unwrap();
    assert_eq!(filtered["total_files"].as_u64().unwrap(), 1);
    assert_eq!(
        filtered["files"][0]["path"].as_str().unwrap(),
        "src/alpha/a.rs"
    );
    assert!(
        filtered.get("components").is_none(),
        "component-filtered view should omit component summary"
    );
}

#[test]
fn file_health_marks_components_unavailable_when_membership_stale() {
    use std::sync::Arc;
    use sutra::tools::ToolContext;

    let (dir, db) = setup_db();

    let fa = seed_file(&db, "src/alpha/a.rs");
    let fb = seed_file(&db, "src/beta/b.rs");
    seed_fn(&db, fa, "alpha::deep", "deep", Some(6));
    seed_fn(&db, fb, "beta::deep", "deep", Some(7));

    // Components + membership exist, but no clustering_meta was ever stamped — the
    // grouping's currency for the live graph/history/config cannot be verified, so
    // the query path must treat it as stale rather than scoring off it (sutra/426).
    db.insert_component("alpha", "Alpha").unwrap();
    db.insert_component("beta", "Beta").unwrap();
    db.batch_insert_membership(&[("alpha".into(), fa), ("beta".into(), fb)])
        .unwrap();

    let findings = compute_nested_complexity(&db).unwrap();
    db.replace_health_findings(&findings).unwrap();
    publish_seeded_run(&db);

    let ctx = ToolContext::for_test(Arc::new(db), dir.path().to_path_buf());
    let result = sutra::tools::file_health::handle_ctx(
        &ctx,
        sutra::health::refresh::DemandOutcome::Refreshed(
            sutra::health::refresh::RefreshResult::Published(sutra::health::evidence::RunId(1)),
        ),
        None,
        None,
        Some("all"),
        None,
        false,
    )
    .unwrap();

    assert!(
        result.get("components").is_none(),
        "stale membership must not surface component scores as current"
    );
    let unavailable = result
        .get("components_unavailable")
        .expect("stale membership should surface a components_unavailable block");
    assert_eq!(unavailable["reason"].as_str().unwrap(), "stale_membership");
    // The per-file evidence is still reported — component unavailability is a
    // distinct axis, not a blanket failure of the report.
    assert_eq!(result["total_files"].as_u64().unwrap(), 2);
}

#[test]
fn file_health_component_instability() {
    let (_dir, db) = setup_db();

    let fa = seed_file(&db, "src/alpha/a.rs");
    let fb = seed_file(&db, "src/beta/b.rs");

    db.insert_component("alpha", "Alpha").unwrap();
    db.insert_component("beta", "Beta").unwrap();
    db.batch_insert_membership(&[("alpha".into(), fa), ("beta".into(), fb)])
        .unwrap();

    db.insert_import(fa, "src/beta/b.rs", Some(fb), 1, "use", None)
        .unwrap();

    // Publish a complete run as a full parse would: these files were analyzed
    // and are genuinely finding-free, so they score a measured 10.0 rather than
    // a partial bound (sutra/416).
    db.replace_health_findings(&[]).unwrap();
    publish_seeded_run(&db);

    let result = sutra::tools::file_health::handle(
        &db,
        health_run::current_verdict(&db),
        None,
        None,
        Some("all"),
        None,
        false,
    )
    .unwrap();
    let components = result["components"].as_array().unwrap();
    assert!(!components.is_empty());

    let alpha = components
        .iter()
        .find(|c| c["name"].as_str().unwrap() == "Alpha")
        .unwrap();
    let inst = &alpha["instability"];
    assert_eq!(inst["ce"].as_u64().unwrap(), 1);
    assert_eq!(inst["ca"].as_u64().unwrap(), 0);
    assert!((inst["value"].as_f64().unwrap() - 1.0).abs() < 1e-9);

    // Alpha is fully unstable (I=1.0) and takes the instability penalty even
    // with no findings; Beta is fully stable (I=0.0) and stays at 10.0.
    let alpha_score = alpha["health_score"].as_f64().unwrap();
    assert!(
        (alpha_score - (10.0 - sutra::health::scoring::instability_penalty(1.0))).abs() < 1e-9,
        "alpha score {alpha_score} should reflect the instability penalty"
    );
    let beta = components
        .iter()
        .find(|c| c["name"].as_str().unwrap() == "Beta")
        .unwrap();
    assert!((beta["health_score"].as_f64().unwrap() - 10.0).abs() < 1e-9);
}

#[test]
fn instability_penalty_is_monotonic_and_bounded() {
    use sutra::health::scoring::instability_penalty;
    assert_eq!(instability_penalty(0.0), 0.0);
    assert!(instability_penalty(0.5) > 0.0);
    assert!(instability_penalty(1.0) > instability_penalty(0.5));
    // Clamped inputs never exceed the input=1.0 penalty.
    assert_eq!(instability_penalty(5.0), instability_penalty(1.0));
}

// --- hrr_shape_change finding conversion ---

#[test]
fn shape_change_findings_only_subtle_structural_with_file_id() {
    use sutra::similarity::diff::{DiffQuadrant, ShapeChange};
    let changes = vec![
        // Qualifies: subtle-structural with a resolved file_id.
        ShapeChange {
            file_path: "src/a.rs".into(),
            symbol_name: "foo".into(),
            symbol_id: Some(7),
            file_id: Some(3),
            text_delta: 0.05,
            hrr_delta: 0.6,
            quadrant: DiffQuadrant::SubtleStructural,
        },
        // Wrong quadrant.
        ShapeChange {
            file_path: "src/b.rs".into(),
            symbol_name: "bar".into(),
            symbol_id: Some(8),
            file_id: Some(4),
            text_delta: 0.5,
            hrr_delta: 0.6,
            quadrant: DiffQuadrant::MajorRewrite,
        },
        // Subtle-structural but unresolved file_id → skipped.
        ShapeChange {
            file_path: "src/c.rs".into(),
            symbol_name: "baz".into(),
            symbol_id: None,
            file_id: None,
            text_delta: 0.05,
            hrr_delta: 0.6,
            quadrant: DiffQuadrant::SubtleStructural,
        },
    ];
    let findings = sutra::health::ondemand::compute_shape_change_findings(&changes, 0.15);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].file_id, 3);
    assert_eq!(findings[0].symbol_id, Some(7));
    assert_eq!(findings[0].biomarker_kind, BiomarkerKind::HrrShapeChange);
    assert_eq!(findings[0].provenance, "on-demand:hrr");
    assert_eq!(findings[0].metric_value, 0.6);
}

// --- ImportCycle biomarker ---

#[test]
fn import_cycle_fires_for_cyclic_files() {
    let (_dir, db) = setup_db();
    let a = seed_file(&db, "src/a.rs");
    let b = seed_file(&db, "src/b.rs");
    let c = seed_file(&db, "src/c.rs");

    // a -> b -> c -> a (triangle cycle)
    db.insert_import(a, "src/b.rs", Some(b), 1, "use", None)
        .unwrap();
    db.insert_import(b, "src/c.rs", Some(c), 1, "use", None)
        .unwrap();
    db.insert_import(c, "src/a.rs", Some(a), 1, "use", None)
        .unwrap();

    let findings = compute_all_health_findings(&db, _dir.path()).unwrap();
    let cycle_findings: Vec<&HealthFinding> = findings
        .iter()
        .filter(|f| f.biomarker_kind == BiomarkerKind::ImportCycle)
        .collect();

    assert_eq!(cycle_findings.len(), 3);
    let mut found_ids: Vec<i64> = cycle_findings.iter().map(|f| f.file_id).collect();
    found_ids.sort();
    assert_eq!(found_ids, vec![a, b, c]);

    for f in &cycle_findings {
        assert_eq!(f.severity, HealthSeverity::Informational);
        assert_eq!(f.confidence, 1.0);
        assert_eq!(f.metric_value, 1.0);
        assert_eq!(f.threshold, 1.0);
    }
}

#[test]
fn import_cycle_absent_for_acyclic() {
    let (_dir, db) = setup_db();
    let a = seed_file(&db, "src/a.rs");
    let b = seed_file(&db, "src/b.rs");
    let c = seed_file(&db, "src/c.rs");

    // a -> b -> c (no back edge)
    db.insert_import(a, "src/b.rs", Some(b), 1, "use", None)
        .unwrap();
    db.insert_import(b, "src/c.rs", Some(c), 1, "use", None)
        .unwrap();

    let findings = compute_all_health_findings(&db, _dir.path()).unwrap();
    let cycle_findings: Vec<&HealthFinding> = findings
        .iter()
        .filter(|f| f.biomarker_kind == BiomarkerKind::ImportCycle)
        .collect();

    assert!(cycle_findings.is_empty());
}

#[test]
fn import_cycle_roundtrips_through_db() {
    let (_dir, db) = setup_db();
    let a = seed_file(&db, "src/a.rs");
    let b = seed_file(&db, "src/b.rs");

    db.insert_import(a, "src/b.rs", Some(b), 1, "use", None)
        .unwrap();
    db.insert_import(b, "src/a.rs", Some(a), 1, "use", None)
        .unwrap();

    let findings = compute_all_health_findings(&db, _dir.path()).unwrap();
    db.replace_health_findings(&findings).unwrap();

    let rows = db.get_health_findings(None, Some("import_cycle")).unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.biomarker_kind, "import_cycle");
        assert_eq!(row.severity, "informational");
    }
}

#[test]
fn snapshot_file_completeness_round_trips() {
    // sutra/408: `partial` + `missing_biomarkers` survive the snapshot so trend
    // can tell partial analysis from real degradation.
    let (_dir, db) = setup_db();
    let snap_id = insert_snapshot(&db, 5.0);
    let files = vec![
        SnapshotFileRow {
            file_id: 1,
            file_path: "src/partial.rs".into(),
            score: 5.0,
            category_scores: "{}".into(),
            completeness: SnapshotCompleteness::Partial,
            missing_biomarkers: vec!["nested_complexity".into(), "import_cycle".into()],
            score_upper: None,
            score_basis: Some("basis-v1".into()),
        },
        SnapshotFileRow {
            file_id: 2,
            file_path: "src/whole.rs".into(),
            score: 9.0,
            category_scores: "{}".into(),
            completeness: SnapshotCompleteness::Complete,
            missing_biomarkers: Vec::new(),
            score_upper: None,
            score_basis: Some("basis-v1".into()),
        },
    ];
    db.insert_snapshot_files(snap_id, &files).unwrap();

    let loaded = db.snapshot_file_scores(snap_id).unwrap();
    let partial = loaded
        .iter()
        .find(|f| f.file_path == "src/partial.rs")
        .unwrap();
    assert_eq!(partial.completeness, SnapshotCompleteness::Partial);
    assert_eq!(
        partial.missing_biomarkers,
        vec!["nested_complexity", "import_cycle"]
    );
    let whole = loaded
        .iter()
        .find(|f| f.file_path == "src/whole.rs")
        .unwrap();
    assert_eq!(whole.completeness, SnapshotCompleteness::Complete);
    assert!(whole.missing_biomarkers.is_empty());
}

// --- sutra/418: trend completeness fidelity ---

fn snap_row(
    file_id: i64,
    path: &str,
    score: f64,
    completeness: SnapshotCompleteness,
    missing: &[&str],
) -> SnapshotFileRow {
    SnapshotFileRow {
        file_id,
        file_path: path.into(),
        score,
        category_scores: "{}".into(),
        completeness,
        missing_biomarkers: missing.iter().map(|m| m.to_string()).collect(),
        score_upper: None,
        score_basis: Some("basis-v1".into()),
    }
}

fn trend(db: &Db, path: Option<&str>) -> serde_json::Value {
    sutra::tools::trend::handle(
        db,
        &sutra::tools::trend::TrendArgs {
            workspace: String::new(),
            from: None,
            to: None,
            path: path.map(Into::into),
            limit: None,
        },
    )
    .unwrap()
}

fn entry<'a>(bucket: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    bucket
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["path"] == path)
}

#[test]
fn trend_comparison_measures_only_complete_pairs_and_surfaces_the_rest() {
    use SnapshotCompleteness::{Complete, Partial, Unknown};
    let (_dir, db) = setup_db();

    let from = insert_snapshot(&db, 7.0);
    db.insert_snapshot_files(
        from,
        &[
            snap_row(1, "src/a.rs", 8.0, Complete, &[]),
            snap_row(2, "src/b.rs", 6.0, Complete, &[]),
            snap_row(3, "src/c.rs", 5.0, Partial, &["import_cycle"]),
            snap_row(4, "src/d.rs", 7.0, Unknown, &[]),
            snap_row(6, "src/f.rs", 9.0, Complete, &[]),
            snap_row(
                7,
                "src/g.rs",
                5.0,
                Partial,
                &["import_cycle", "hidden_coupling"],
            ),
        ],
    )
    .unwrap();
    let to = insert_snapshot(&db, 8.0);
    db.insert_snapshot_files(
        to,
        &[
            // Complete → complete, score up: the only measured change.
            snap_row(1, "src/a.rs", 9.0, Complete, &[]),
            // Equal score, complete → partial: a completeness transition.
            snap_row(2, "src/b.rs", 6.0, Partial, &["dead_code_ratio"]),
            // Partial → complete, score up: NOT a measured improvement.
            snap_row(3, "src/c.rs", 8.0, Complete, &[]),
            // Legacy unknown → complete: NOT a measured improvement.
            snap_row(4, "src/d.rs", 9.0, Complete, &[]),
            // New file: no fallback baseline of 10.
            snap_row(5, "src/e.rs", 4.0, Complete, &[]),
            // Same partial observation (missing set reordered): no change.
            snap_row(
                7,
                "src/g.rs",
                5.0,
                Partial,
                &["hidden_coupling", "import_cycle"],
            ),
        ],
    )
    .unwrap();

    let result = trend(&db, None);
    let files = &result["files"];

    let improved = files["improved"].as_array().unwrap();
    assert_eq!(improved.len(), 1, "{improved:?}");
    assert_eq!(improved[0]["path"], "src/a.rs");
    assert_eq!(improved[0]["from_completeness"]["completeness"], "complete");
    assert_eq!(improved[0]["to_completeness"]["partial"], false);
    assert!(files["degraded"].as_array().unwrap().is_empty());

    let inc = &files["incomparable"];
    assert_eq!(inc.as_array().unwrap().len(), 5, "{inc}");

    let b = entry(inc, "src/b.rs").expect("equal-score transition is visible");
    assert_eq!(b["reason"], "partial");
    assert_eq!(b["completeness_changed"], true);
    assert_eq!(b["from"], 6.0);
    assert_eq!(b["to"], 6.0);
    assert!(b.get("delta").is_none(), "incomparable carries no delta");
    assert_eq!(b["from_completeness"]["partial"], false);
    assert_eq!(b["to_completeness"]["partial"], true);
    assert_eq!(
        b["to_completeness"]["missing_biomarkers"],
        serde_json::json!(["dead_code_ratio"])
    );

    let c = entry(inc, "src/c.rs").unwrap();
    assert_eq!(c["reason"], "partial");
    assert_eq!(
        c["from_completeness"]["missing_biomarkers"],
        serde_json::json!(["import_cycle"])
    );

    let d = entry(inc, "src/d.rs").unwrap();
    assert_eq!(d["reason"], "unknown_completeness");
    assert_eq!(d["from_completeness"]["completeness"], "unknown");
    assert!(
        d["from_completeness"]["partial"].is_null(),
        "legacy cannot prove completeness"
    );

    let e = entry(inc, "src/e.rs").unwrap();
    assert_eq!(e["reason"], "new_file");
    assert!(e["from"].is_null() && e["from_completeness"].is_null());

    let f = entry(inc, "src/f.rs").unwrap();
    assert_eq!(f["reason"], "removed_file");
    assert!(f["to"].is_null());

    assert!(
        entry(inc, "src/g.rs").is_none(),
        "unchanged partial observation"
    );

    // Aggregates move with the incomplete files, so they are not measured.
    assert!(result["deltas"]["health_score"].is_null());
    assert_eq!(result["aggregate_comparison"]["measured"], false);
    assert_eq!(
        result["aggregate_comparison"]["reason"],
        "incomplete_evidence"
    );
    // Parse counters are still reported.
    assert!(result["deltas"]["files_parsed"].is_number());

    assert_eq!(result["completeness"]["from"]["partial"], 2);
    assert_eq!(result["completeness"]["from"]["unknown"], 1);
    assert_eq!(result["completeness"]["to"]["complete"], 4);
    assert_eq!(result["completeness"]["to"]["partial"], 2);
}

#[test]
fn trend_history_exposes_completeness_on_every_entry() {
    let (_dir, db) = setup_db();
    for (score, completeness, missing) in [
        (7.0, SnapshotCompleteness::Unknown, &[][..]),
        (5.0, SnapshotCompleteness::Partial, &["import_cycle"][..]),
        (9.0, SnapshotCompleteness::Complete, &[][..]),
    ] {
        let id = insert_snapshot(&db, score);
        db.insert_snapshot_files(id, &[snap_row(1, "src/x.rs", score, completeness, missing)])
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let result = trend(&db, Some("src/x.rs"));
    let entries = result["snapshots"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    // Newest first.
    assert_eq!(entries[0]["completeness"], "complete");
    assert_eq!(entries[0]["partial"], false);
    assert_eq!(entries[1]["completeness"], "partial");
    assert_eq!(entries[1]["partial"], true);
    assert_eq!(
        entries[1]["missing_biomarkers"],
        serde_json::json!(["import_cycle"])
    );
    assert_eq!(entries[2]["completeness"], "unknown");
    assert!(entries[2]["partial"].is_null());
    for e in entries {
        assert!(e["missing_biomarkers"].is_array());
        // Only a complete observation is a measured score (sutra/438).
        assert_eq!(
            e["health_score"].is_number(),
            e["completeness"] == "complete"
        );
    }
}

#[test]
fn legacy_snapshot_rows_read_as_unknown_not_complete() {
    // A row written without completeness columns — pre-0073 data, or the
    // pre-sutra/418 atomic writer — carries only the column defaults.
    let (_dir, db) = setup_db();
    let from = insert_snapshot(&db, 6.0);
    db.conn_for_test()
        .execute(
            "INSERT INTO health_snapshot_files
             (snapshot_id, file_id, file_path, score, category_scores)
             VALUES (?1, 1, 'src/old.rs', 6.0, '{}')",
            [from],
        )
        .unwrap();
    let to = insert_snapshot(&db, 8.0);
    db.insert_snapshot_files(
        to,
        &[snap_row(
            1,
            "src/old.rs",
            8.0,
            SnapshotCompleteness::Complete,
            &[],
        )],
    )
    .unwrap();

    let rows = db.snapshot_file_scores(from).unwrap();
    assert_eq!(rows[0].completeness, SnapshotCompleteness::Unknown);

    let result = trend(&db, None);
    assert!(result["files"]["improved"].as_array().unwrap().is_empty());
    let old = entry(&result["files"]["incomparable"], "src/old.rs").unwrap();
    assert_eq!(old["reason"], "unknown_completeness");
    assert_eq!(old["from"], 6.0);
    assert_eq!(old["to"], 8.0);
}

#[test]
fn trend_aggregates_are_incomparable_when_the_population_changes() {
    use SnapshotCompleteness::Complete;
    let (_dir, db) = setup_db();
    let from = insert_snapshot(&db, 8.0);
    db.insert_snapshot_files(
        from,
        &[SnapshotFileRow {
            category_scores: r#"{"structural":2.0}"#.into(),
            ..snap_row(1, "src/a.rs", 8.0, Complete, &[])
        }],
    )
    .unwrap();
    let to = insert_snapshot(&db, 9.0);
    db.insert_snapshot_files(
        to,
        &[
            SnapshotFileRow {
                category_scores: r#"{"structural":2.0}"#.into(),
                ..snap_row(1, "src/a.rs", 8.0, Complete, &[])
            },
            // A clean new file lifts the workspace mean without any code improving.
            snap_row(2, "src/new.rs", 10.0, Complete, &[]),
        ],
    )
    .unwrap();

    let result = trend(&db, None);
    assert_eq!(
        result["aggregate_comparison"]["reason"],
        "population_changed"
    );
    assert!(result["deltas"]["health_score"].is_null());
    let structural = &result["categories"]["structural"];
    assert_eq!(structural["from"], 2.0);
    assert_eq!(structural["to"], 2.0);
    assert!(structural["delta"].is_null());
    assert!(result["files"]["improved"].as_array().unwrap().is_empty());
}

// --- sutra/416: component deltas and partial reports ---

fn comp_row(
    id: &str,
    score: f64,
    completeness: SnapshotCompleteness,
    basis: Option<&str>,
) -> SnapshotComponentRow {
    SnapshotComponentRow {
        component_id: id.into(),
        component_name: id.to_uppercase(),
        score,
        member_count: 2,
        total_nloc: 100,
        completeness,
        score_basis: basis.map(Into::into),
        members: Some(vec![SnapshotComponentMember {
            file_path: format!("src/{id}.rs"),
            weight: 100,
        }]),
        instability_penalty: Some(0.0),
    }
}

#[test]
fn trend_component_deltas_are_measured_only_under_a_matching_basis() {
    use SnapshotCompleteness::{Complete, Partial, Unknown};
    let (_dir, db) = setup_db();
    let from = insert_snapshot(&db, 8.0);
    db.insert_snapshot_components(
        from,
        &[
            comp_row(
                "same",
                component_score(&[(8.0, 100)], 0.0),
                Complete,
                Some("m1"),
            ),
            comp_row("moved", 8.0, Complete, Some("m1")),
            comp_row("legacy", 8.0, Unknown, None),
            comp_row("half", 8.0, Complete, Some("m1")),
            comp_row("gone", 8.0, Complete, Some("m1")),
        ],
    )
    .unwrap();
    db.insert_snapshot_files(from, &[snap_row(1, "src/same.rs", 8.0, Complete, &[])])
        .unwrap();
    let to = insert_snapshot(&db, 8.0);
    db.insert_snapshot_files(to, &[snap_row(1, "src/same.rs", 7.0, Complete, &[])])
        .unwrap();
    db.insert_snapshot_components(
        to,
        &[
            comp_row(
                "same",
                component_score(&[(7.0, 100)], 0.0),
                Complete,
                Some("m1"),
            ),
            comp_row("moved", 9.5, Complete, Some("m2")),
            comp_row("legacy", 9.0, Complete, Some("m1")),
            comp_row("half", 9.0, Partial, Some("m1")),
            comp_row("fresh", 6.0, Complete, Some("m1")),
        ],
    )
    .unwrap();

    let out = trend(&db, None);
    let comps = out["components"].as_array().unwrap();
    let get = |id: &str| {
        comps
            .iter()
            .find(|c| c["id"] == id)
            .unwrap_or_else(|| panic!("{id} listed: {out}"))
    };
    let same = get("same");
    assert_eq!(same["measured"], true);
    let delta = component_score(&[(7.0, 100)], 0.0) - component_score(&[(8.0, 100)], 0.0);
    assert_eq!(same["measured_delta"], round2(delta));
    assert!(delta < 0.0);
    assert_eq!(same["weight_shift"], 0.0);
    for (id, reason) in [
        ("moved", "score_basis_changed"),
        ("legacy", "unknown_completeness"),
        ("half", "partial"),
        ("fresh", "new_component"),
        ("gone", "removed_component"),
    ] {
        let c = get(id);
        assert_eq!(c["measured"], false, "{id}");
        assert_eq!(c["reason"], reason, "{id}");
        assert!(
            c["measured_delta"].is_null() && c["weight_shift"].is_null(),
            "{id}: no delta without a measurement"
        );
    }
    assert!(
        get("fresh")["from"].is_null(),
        "a new component has no fallback baseline of 10.0"
    );
}

// --- sutra/436: component deltas at fixed baseline weights ---

/// A complete component row under basis "m1" with the given members
/// `(path, weight)` and instability penalty.
fn weighted_comp(
    id: &str,
    score: f64,
    members: Option<&[(&str, i64)]>,
    penalty: Option<f64>,
) -> SnapshotComponentRow {
    SnapshotComponentRow {
        members: members.map(|ms| {
            ms.iter()
                .map(|&(path, weight)| SnapshotComponentMember {
                    file_path: path.into(),
                    weight,
                })
                .collect()
        }),
        instability_penalty: penalty,
        ..comp_row(id, score, SnapshotCompleteness::Complete, Some("m1"))
    }
}

/// Complete file rows for `(path, score)` pairs.
fn complete_files(scores: &[(&str, f64)]) -> Vec<SnapshotFileRow> {
    scores
        .iter()
        .enumerate()
        .map(|(i, &(path, score))| {
            snap_row(
                i as i64 + 1,
                path,
                score,
                SnapshotCompleteness::Complete,
                &[],
            )
        })
        .collect()
}

#[test]
fn trend_component_deltas_are_measured_at_baseline_weights() {
    let (_dir, db) = setup_db();
    let from = insert_snapshot(&db, 8.0);
    db.insert_snapshot_files(
        from,
        &complete_files(&[
            ("mix/a.rs", 10.0),
            ("mix/b.rs", 6.0),
            ("debt/a.rs", 10.0),
            ("debt/b.rs", 6.0),
            ("unstable/a.rs", 9.0),
            ("pre/a.rs", 9.0),
            ("noinst/a.rs", 9.0),
        ]),
    )
    .unwrap();
    db.insert_snapshot_components(
        from,
        &[
            weighted_comp(
                "mix",
                component_score(&[(10.0, 100), (6.0, 100)], 0.0),
                Some(&[("mix/a.rs", 100), ("mix/b.rs", 100)]),
                Some(0.0),
            ),
            weighted_comp(
                "debt",
                component_score(&[(10.0, 100), (6.0, 300)], 0.0),
                Some(&[("debt/a.rs", 100), ("debt/b.rs", 300)]),
                Some(0.0),
            ),
            weighted_comp(
                "unstable",
                component_score(&[(9.0, 100)], 0.0),
                Some(&[("unstable/a.rs", 100)]),
                Some(0.0),
            ),
            weighted_comp("pre", 9.0, None, None),
            weighted_comp("noinst", 9.0, Some(&[("noinst/a.rs", 100)]), Some(0.0)),
        ],
    )
    .unwrap();

    let to = insert_snapshot(&db, 8.0);
    db.insert_snapshot_files(
        to,
        &complete_files(&[
            ("mix/a.rs", 10.0),
            ("mix/b.rs", 6.0),
            ("debt/a.rs", 8.0),
            ("debt/b.rs", 6.0),
            ("unstable/a.rs", 9.0),
            ("pre/a.rs", 9.0),
            ("noinst/a.rs", 9.0),
        ]),
    )
    .unwrap();
    db.insert_snapshot_components(
        to,
        &[
            // Weight only: mix/a.rs grew (comments) to 300 lines.
            weighted_comp(
                "mix",
                component_score(&[(10.0, 300), (6.0, 100)], 0.0),
                Some(&[("mix/a.rs", 300), ("mix/b.rs", 100)]),
                Some(0.0),
            ),
            // Real debt in debt/a.rs (10 → 8) while debt/b.rs shrank to 100 lines.
            weighted_comp(
                "debt",
                component_score(&[(8.0, 100), (6.0, 100)], 0.0),
                Some(&[("debt/a.rs", 100), ("debt/b.rs", 100)]),
                Some(0.0),
            ),
            weighted_comp(
                "unstable",
                component_score(&[(9.0, 100)], 0.3),
                Some(&[("unstable/a.rs", 100)]),
                Some(0.3),
            ),
            weighted_comp("pre", 9.0, Some(&[("pre/a.rs", 100)]), Some(0.0)),
            weighted_comp("noinst", 9.0, Some(&[("noinst/a.rs", 100)]), None),
        ],
    )
    .unwrap();

    let out = trend(&db, None);
    let comps = out["components"].as_array().unwrap();
    let get = |id: &str| {
        comps
            .iter()
            .find(|c| c["id"] == id)
            .unwrap_or_else(|| panic!("{id} listed: {out}"))
    };

    let mix = get("mix");
    assert_eq!(mix["measured"], true, "{mix}");
    assert_eq!(
        mix["measured_delta"], 0.0,
        "a line-count change is no quality change"
    );
    let mix_shift = component_score(&[(10.0, 300), (6.0, 100)], 0.0)
        - component_score(&[(10.0, 100), (6.0, 100)], 0.0);
    assert!(
        mix_shift > 0.0,
        "diluting clean lines still lift the density term"
    );
    assert_eq!(mix["weight_shift"], round2(mix_shift), "{mix}");

    let debt = get("debt");
    assert_eq!(debt["measured"], true, "{debt}");
    // Measured at baseline weights (100/300); the rest is the weight shift.
    let base = component_score(&[(10.0, 100), (6.0, 300)], 0.0);
    let measured = component_score(&[(8.0, 100), (6.0, 300)], 0.0) - base;
    let observed = component_score(&[(8.0, 100), (6.0, 100)], 0.0) - base;
    assert_eq!(debt["measured_delta"], round2(measured), "{debt}");
    assert_eq!(debt["weight_shift"], round2(observed - measured), "{debt}");

    let unstable = get("unstable");
    assert_eq!(
        unstable["measured_delta"], -0.3,
        "known-both-sides penalty change is measured"
    );
    assert_eq!(unstable["weight_shift"], 0.0);

    for (id, reason) in [
        ("pre", "unknown_weights"),
        ("noinst", "unknown_instability"),
    ] {
        let c = get(id);
        assert_eq!(c["measured"], false, "{id}");
        assert_eq!(c["reason"], reason, "{id}");
        assert!(
            c["measured_delta"].is_null() && c["weight_shift"].is_null(),
            "{id}"
        );
    }

    let order: Vec<&str> = comps.iter().filter_map(|c| c["id"].as_str()).collect();
    assert_eq!(
        &order[..3],
        ["debt", "unstable", "mix"],
        "measured entries sort by measured_delta, worst first"
    );
}

#[test]
fn file_health_reports_bounds_not_a_point_score_for_partial_files() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/hot.rs");
    db.replace_health_findings(&[nested_finding(fid)]).unwrap();
    publish_run_with(&db, |_, kind| {
        (kind == BiomarkerKind::ChangeEntropy)
            .then_some(ProducerOutcome::Missing(MissingReason::NoHistory))
    });
    let out = sutra::tools::file_health::handle(
        &db,
        health_run::current_verdict(&db),
        None,
        None,
        Some("all"),
        None,
        false,
    )
    .unwrap();
    let f = &out["files"][0];
    assert!(f["health_score"].is_null(), "{f}");
    assert_eq!(f["partial"], true);
    let upper = 10.0 - sat(HealthCategory::Structural, 1.34);
    assert!((f["score_bounds"]["upper"].as_f64().unwrap() - round2(upper)).abs() < 1e-9);
    // Organizational saturated at its cap (3.5) on top of the known structural debt.
    assert!((f["score_bounds"]["lower"].as_f64().unwrap() - round2(upper - 3.5)).abs() < 1e-9);
    assert_eq!(
        f["missing_biomarkers"],
        serde_json::json!(["change_entropy"])
    );
    assert_eq!(f["missing"][0]["reason"], "NoHistory");
}

// --- sutra/416 review fixes ---

#[test]
fn a_verdict_for_one_run_does_not_vouch_for_a_newer_one() {
    use sutra::health::assess::PersistentEvidence;
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/hot.rs");
    db.replace_health_findings(&[nested_finding(fid)]).unwrap();
    publish_seeded_run(&db);
    let vouched = health_run::current_verdict(&db);
    // A concurrent refresh publishes another run before the reader loads.
    publish_seeded_run(&db);
    let ev = PersistentEvidence::load(&db, vouched).unwrap();
    let s = ev.file("src/hot.rs").unwrap().score();
    assert!(s.partial(), "the loaded run was never validated");
    assert!(
        s.missing
            .iter()
            .all(|m| m.reason == MissingReason::InputsChanged)
    );
}

#[test]
fn a_complete_outcome_disagreeing_with_retained_findings_is_invalid_evidence() {
    use sutra::health::assess::PersistentEvidence;
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/hot.rs");
    db.replace_health_findings(&[nested_finding(fid)]).unwrap();
    publish_run_with(&db, |_, kind| {
        (kind == BiomarkerKind::NestedComplexity)
            .then_some(ProducerOutcome::Complete { finding_count: 3 })
    });
    let ev = PersistentEvidence::load(&db, health_run::current_verdict(&db)).unwrap();
    let s = ev.file("src/hot.rs").unwrap().score();
    let m = s
        .missing
        .iter()
        .find(|m| m.biomarker == BiomarkerKind::NestedComplexity)
        .expect("inconsistent producer is missing");
    assert_eq!(m.reason, MissingReason::InvalidEvidence);
    assert!(s.deductions.is_empty());
}

#[test]
fn stale_component_membership_is_never_measured() {
    use sutra::health::assess::{PersistentEvidence, score_workspace};
    let (_dir, db) = setup_db();
    let fa = seed_file(&db, "src/alpha/a.rs");
    db.insert_component("alpha", "Alpha").unwrap();
    db.batch_insert_membership(&[("alpha".into(), fa)]).unwrap();
    db.replace_health_findings(&[]).unwrap();
    publish_seeded_run(&db);
    let ev = PersistentEvidence::load(&db, health_run::current_verdict(&db)).unwrap();

    let current = score_workspace(&db, &ev, true).unwrap();
    assert!(current.components[0].value.is_measured());
    let stale = score_workspace(&db, &ev, false).unwrap();
    assert!(
        !stale.components[0].value.is_measured(),
        "stale membership keeps bounds but is never a measurement"
    );
    assert_eq!(current.components[0].basis, stale.components[0].basis);
}
