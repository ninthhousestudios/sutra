use sutra::db::{
    CommitRow, Db, HealthFindingRow, InsertSymbolParams, SnapshotComponentRow, SnapshotFileRow,
    SnapshotParams,
};
use sutra::git::parse_blame_porcelain;
use sutra::health::findings::HealthFinding;
use sutra::health::{
    BiomarkerKind, HealthSeverity, compute_all_health_findings, compute_change_entropy,
    compute_co_change_scatter, compute_hidden_coupling, compute_nested_complexity,
    compute_ownership_risk, instability::compute_component_instability, score_component,
    score_file,
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

    let findings = compute_ownership_risk(&db, dir.path()).unwrap();
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

    let findings = compute_ownership_risk(&db, dir.path()).unwrap();
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

    let findings = compute_ownership_risk(&db, dir.path()).unwrap();
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

    let findings = compute_ownership_risk(&db, dir.path()).unwrap();
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
        },
        SnapshotFileRow {
            file_id: 2,
            file_path: "src/bar.rs".into(),
            score: 6.1,
            category_scores: r#"{"organizational":2.5,"structural":1.4}"#.into(),
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
        },
        SnapshotComponentRow {
            component_id: "comp_b".into(),
            component_name: "db".into(),
            score: 6.9,
            member_count: 3,
            total_nloc: 800,
        },
    ];
    db.insert_snapshot_components(snap_id, &comps).unwrap();

    let loaded = db.snapshot_component_scores(snap_id).unwrap();
    assert_eq!(loaded.len(), 2);

    let auth = loaded.iter().find(|c| c.component_id == "comp_a").unwrap();
    assert!((auth.score - 8.3).abs() < 0.01);
    assert_eq!(auth.member_count, 5);
    assert_eq!(auth.total_nloc, 1200);

    let db_comp = loaded.iter().find(|c| c.component_id == "comp_b").unwrap();
    assert!((db_comp.score - 6.9).abs() < 0.01);
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
        }],
    )
    .unwrap();

    let history = db.file_health_history("src/main.rs", 10).unwrap();
    assert_eq!(history.len(), 3);
    // Newest first
    assert!((history[0].1 - 9.1).abs() < 0.01);
    assert!((history[1].1 - 8.2).abs() < 0.01);
    assert!((history[2].1 - 7.5).abs() < 0.01);

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
            },
            SnapshotFileRow {
                file_id: 2,
                file_path: "src/b.rs".into(),
                score: 6.0,
                category_scores: r#"{"organizational":2.0}"#.into(),
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
            },
            SnapshotFileRow {
                file_id: 2,
                file_path: "src/b.rs".into(),
                score: 5.0,
                category_scores: r#"{"organizational":3.0}"#.into(),
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

    // Aggregate health delta
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

// --- Health delta ---

#[test]
fn test_health_delta_degradation() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/hotfile.rs");
    seed_fn(&db, fid, "hotfile::process", "process", Some(5));

    // Snapshot with good score
    let snap_id = insert_snapshot(&db, 9.0);
    db.insert_snapshot_files(
        snap_id,
        &[SnapshotFileRow {
            file_id: fid,
            file_path: "src/hotfile.rs".into(),
            score: 9.0,
            category_scores: "{}".into(),
        }],
    )
    .unwrap();

    // Add a finding that degrades the score
    db.replace_health_findings(&[HealthFinding {
        file_id: fid,
        symbol_id: None,
        biomarker_kind: BiomarkerKind::NestedComplexity,
        severity: HealthSeverity::Advisory,
        confidence: 1.0,
        provenance: "computed".into(),
        metric_value: 6.0,
        threshold: 4.0,
        detail: "deep nesting".into(),
    }])
    .unwrap();

    let delta =
        sutra::health::ondemand::compute_health_delta(&db, &["src/hotfile.rs".to_string()], &[])
            .unwrap();

    assert_eq!(delta.degraded.len(), 1);
    assert!(delta.improved.is_empty());
    let entry = &delta.degraded[0];
    assert_eq!(entry.path, "src/hotfile.rs");
    assert!((entry.previous_score - 9.0).abs() < 0.01);
    assert!(entry.current_score < 9.0);
    assert!(entry.delta < 0.0);
}

#[test]
fn test_health_delta_improvement() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/cleaned.rs");
    seed_fn(&db, fid, "cleaned::run", "run", Some(2));

    // Snapshot with poor score
    let snap_id = insert_snapshot(&db, 6.0);
    db.insert_snapshot_files(
        snap_id,
        &[SnapshotFileRow {
            file_id: fid,
            file_path: "src/cleaned.rs".into(),
            score: 6.0,
            category_scores: r#"{"structural":2.0}"#.into(),
        }],
    )
    .unwrap();

    // No findings → current score = 10.0 (base)
    let delta =
        sutra::health::ondemand::compute_health_delta(&db, &["src/cleaned.rs".to_string()], &[])
            .unwrap();

    assert!(delta.degraded.is_empty());
    assert_eq!(delta.improved.len(), 1);
    let entry = &delta.improved[0];
    assert_eq!(entry.path, "src/cleaned.rs");
    assert!(entry.delta > 0.0);
}

#[test]
fn test_health_delta_no_snapshot_uses_base_10() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/new.rs");
    seed_fn(&db, fid, "new::init", "init", Some(2));

    // No snapshot exists → previous defaults to 10.0
    // No findings → current = 10.0 → no delta
    let delta =
        sutra::health::ondemand::compute_health_delta(&db, &["src/new.rs".to_string()], &[])
            .unwrap();

    assert!(delta.degraded.is_empty());
    assert!(delta.improved.is_empty());
}

#[test]
fn test_health_delta_with_ondemand_findings() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/volatile.rs");
    seed_fn(&db, fid, "volatile::handle", "handle", Some(2));

    // Snapshot with good score
    let snap_id = insert_snapshot(&db, 9.5);
    db.insert_snapshot_files(
        snap_id,
        &[SnapshotFileRow {
            file_id: fid,
            file_path: "src/volatile.rs".into(),
            score: 9.5,
            category_scores: "{}".into(),
        }],
    )
    .unwrap();

    // On-demand finding
    let ondemand = vec![HealthFinding {
        file_id: fid,
        symbol_id: Some(1),
        biomarker_kind: BiomarkerKind::FunctionHotspot,
        severity: HealthSeverity::Advisory,
        confidence: 1.0,
        provenance: "on-demand:blame".into(),
        metric_value: 12.0,
        threshold: 5.0,
        detail: "volatile::handle: 12 distinct commits".into(),
    }];

    let delta = sutra::health::ondemand::compute_health_delta(
        &db,
        &["src/volatile.rs".to_string()],
        &ondemand,
    )
    .unwrap();

    assert_eq!(delta.degraded.len(), 1);
    let entry = &delta.degraded[0];
    assert!(entry.delta < 0.0);
    assert!(!entry.driving_findings.is_empty());
    assert_eq!(entry.driving_findings[0].biomarker_kind, "function_hotspot");
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
// Orient health section
// ---------------------------------------------------------------------------

#[test]
fn orient_includes_health_section() {
    let (dir, db) = setup_db();

    let fa = seed_file(&db, "src/tools/orient.rs");
    seed_fn(&db, fa, "orient::handle", "handle", Some(6));

    db.insert_component("comp1", "Tools").unwrap();
    db.batch_insert_membership(&[("comp1".into(), fa)]).unwrap();

    // Generate health findings (nesting > 4 threshold)
    let findings = compute_nested_complexity(&db).unwrap();
    assert!(!findings.is_empty());
    db.replace_health_findings(&findings).unwrap();

    let result = sutra::tools::orient::handle(
        &db,
        "Tools",
        dir.path(),
        None,
        None,
        &sutra::parser::adapter::default_registry(),
    )
    .unwrap();
    let orientation = result["orientation"].as_array().unwrap();
    assert_eq!(orientation.len(), 1);

    let section = &orientation[0];
    assert!(
        section.get("health").is_some(),
        "health section should be present"
    );

    let health = &section["health"];
    assert!(health["health_score"].as_f64().is_some());
    assert!(!health["top_findings"].as_array().unwrap().is_empty());

    let finding = &health["top_findings"][0];
    assert_eq!(finding["biomarker"].as_str().unwrap(), "nested_complexity");
    assert_eq!(finding["severity"].as_str().unwrap(), "advisory");
}

#[test]
fn orient_health_absent_when_clean() {
    let (dir, db) = setup_db();

    let fa = seed_file(&db, "src/clean.rs");
    seed_fn(&db, fa, "clean::foo", "foo", Some(1));

    db.insert_component("comp1", "Clean").unwrap();
    db.batch_insert_membership(&[("comp1".into(), fa)]).unwrap();

    let result = sutra::tools::orient::handle(
        &db,
        "Clean",
        dir.path(),
        None,
        None,
        &sutra::parser::adapter::default_registry(),
    )
    .unwrap();
    let section = &result["orientation"][0];

    // Health section present but with perfect score and no findings
    let health = &section["health"];
    assert!((health["health_score"].as_f64().unwrap() - 10.0).abs() < 0.01);
    assert!(
        health.get("top_findings").is_none()
            || health["top_findings"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true)
    );
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

    // Without filter: both files + component summary
    let all = sutra::tools::file_health::handle(&db, None, None, None, None, false).unwrap();
    assert_eq!(all["total_files"].as_u64().unwrap(), 2);
    assert!(
        all.get("components").is_some(),
        "unfiltered should include component scores"
    );

    // With component filter: only Alpha's file, no component summary
    let filtered =
        sutra::tools::file_health::handle(&db, None, None, None, Some("Alpha"), false).unwrap();
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

    let result =
        sutra::tools::file_health::handle(&db, None, None, Some("all"), None, false).unwrap();
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
}

// --- ImportCycle biomarker ---

#[test]
fn import_cycle_fires_for_cyclic_files() {
    let (_dir, db) = setup_db();
    let a = seed_file(&db, "src/a.rs");
    let b = seed_file(&db, "src/b.rs");
    let c = seed_file(&db, "src/c.rs");

    // a -> b -> c -> a (triangle cycle)
    db.insert_import(a, "src/b.rs", Some(b), 1, "use", None).unwrap();
    db.insert_import(b, "src/c.rs", Some(c), 1, "use", None).unwrap();
    db.insert_import(c, "src/a.rs", Some(a), 1, "use", None).unwrap();

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
    db.insert_import(a, "src/b.rs", Some(b), 1, "use", None).unwrap();
    db.insert_import(b, "src/c.rs", Some(c), 1, "use", None).unwrap();

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

    db.insert_import(a, "src/b.rs", Some(b), 1, "use", None).unwrap();
    db.insert_import(b, "src/a.rs", Some(a), 1, "use", None).unwrap();

    let findings = compute_all_health_findings(&db, _dir.path()).unwrap();
    db.replace_health_findings(&findings).unwrap();

    let rows = db.get_health_findings(None, Some("import_cycle")).unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.biomarker_kind, "import_cycle");
        assert_eq!(row.severity, "informational");
    }
}
