use sutra::db::{CommitRow, Db, InsertSymbolParams, SnapshotParams};
use sutra::health::findings::HealthFinding;
use sutra::health::{
    BiomarkerKind, HealthSeverity, compute_all_health_findings, compute_blast_radius_churn,
    compute_change_entropy, compute_co_change_scatter, compute_hidden_coupling,
    compute_nested_complexity, compute_ownership_risk,
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
    for kind in BiomarkerKind::ALL {
        assert_eq!(BiomarkerKind::parse(kind.as_str()), Some(kind));
    }
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

    let owners = sutra::health::git_metrics::load_owners(dir.path()).expect("owners config");
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

    let owners = sutra::health::git_metrics::load_owners(dir.path()).expect("owners config");
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

    let owners = sutra::health::git_metrics::load_owners(dir.path()).expect("owners config");
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

    let owners = sutra::health::git_metrics::load_owners(dir.path()).expect("owners config");
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

// --- Snapshot storage ---

#[test]
fn test_snapshot_pattern_family_count_roundtrip() {
    let (_dir, db) = setup_db();
    db.insert_snapshot(&SnapshotParams {
        pattern_family_count: 3,
        ..Default::default()
    })
    .unwrap();

    let snaps = db.latest_snapshots(1).unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].pattern_family_count, 3);
}
