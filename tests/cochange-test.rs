use std::path::Path;

use sutra::db::entity_changes::EntityChangeRow;
use sutra::db::{CommitRow, Db};
use sutra::git::git_commit_files;

fn sutra_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

#[test]
fn git_commit_files_returns_data() {
    let results = git_commit_files(sutra_root(), 90).unwrap();
    assert!(!results.is_empty(), "should return commit-file pairs");
    let first = &results[0];
    assert!(!first.hash.is_empty());
    assert!(first.timestamp > 0);
    assert!(!first.author.is_empty());
    assert!(!first.path.is_empty());
}

#[test]
fn git_commit_files_has_unique_hash_per_group() {
    let results = git_commit_files(sutra_root(), 90).unwrap();
    let mut seen_paths_per_commit: std::collections::HashMap<
        &str,
        std::collections::HashSet<&str>,
    > = std::collections::HashMap::new();
    for cf in &results {
        let inserted = seen_paths_per_commit
            .entry(&cf.hash)
            .or_default()
            .insert(&cf.path);
        assert!(
            inserted,
            "duplicate (hash, path) pair: ({}, {})",
            cf.hash, cf.path
        );
    }
}

#[test]
fn cochange_for_file_returns_partners() {
    let (_dir, db) = setup_db();

    let f1 = db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    let f2 = db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();
    let f3 = db.upsert_file("src/c.rs", "rust", "h3", 10, true).unwrap();

    let commits = vec![
        CommitRow {
            hash: "c1".into(),
            committed_at: 1000,
            author: "a@b.c".into(),
        },
        CommitRow {
            hash: "c2".into(),
            committed_at: 1001,
            author: "a@b.c".into(),
        },
        CommitRow {
            hash: "c3".into(),
            committed_at: 1002,
            author: "a@b.c".into(),
        },
    ];
    // f1 and f2 share c1, c2; f1 and f3 share c1 only; f1 also in c3 alone
    let pairs = vec![
        ("c1".into(), f1),
        ("c1".into(), f2),
        ("c1".into(), f3),
        ("c2".into(), f1),
        ("c2".into(), f2),
        ("c3".into(), f1),
    ];
    db.replace_commit_files(&commits, &pairs).unwrap();

    let partners = db.cochange_for_file(f1, 0.1).unwrap();
    assert!(!partners.is_empty(), "should find cochange partners");

    let f2_entry = partners.iter().find(|(p, _, _)| p == "src/b.rs");
    assert!(f2_entry.is_some(), "f2 should be a partner of f1");
    let (_, jaccard, shared) = f2_entry.unwrap();
    // shared=2, f1 has 3 commits, f2 has 2 commits → jaccard = 2/(3+2-2) = 2/3 ≈ 0.667
    assert_eq!(*shared, 2);
    assert!(
        (*jaccard - 2.0 / 3.0).abs() < 0.01,
        "jaccard should be ~0.667, got {jaccard}"
    );

    let f3_entry = partners.iter().find(|(p, _, _)| p == "src/c.rs");
    assert!(f3_entry.is_some(), "f3 should be a partner of f1");
    let (_, jaccard, shared) = f3_entry.unwrap();
    // shared=1, f1 has 3, f3 has 1 → jaccard = 1/(3+1-1) = 1/3 ≈ 0.333
    assert_eq!(*shared, 1);
    assert!(
        (*jaccard - 1.0 / 3.0).abs() < 0.01,
        "jaccard should be ~0.333, got {jaccard}"
    );
}

#[test]
fn cochange_for_file_excludes_self() {
    let (_dir, db) = setup_db();

    let f1 = db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    let f2 = db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();

    let commits = vec![CommitRow {
        hash: "c1".into(),
        committed_at: 1000,
        author: "a@b.c".into(),
    }];
    let pairs = vec![("c1".into(), f1), ("c1".into(), f2)];
    db.replace_commit_files(&commits, &pairs).unwrap();

    let partners = db.cochange_for_file(f1, 0.0).unwrap();
    assert!(
        !partners.iter().any(|(p, _, _)| p == "src/a.rs"),
        "queried file should be excluded from results"
    );
}

#[test]
fn cochange_for_file_respects_threshold() {
    let (_dir, db) = setup_db();

    let f1 = db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    let f2 = db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();

    // f1 has 10 commits, f2 has 10, they share 1 → jaccard ≈ 0.053
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..10 {
        let hash = format!("a{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: i,
            author: "x".into(),
        });
        pairs.push((hash, f1));
    }
    for i in 0..10 {
        let hash = format!("b{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: i,
            author: "x".into(),
        });
        pairs.push((hash, f2));
    }
    commits.push(CommitRow {
        hash: "shared".into(),
        committed_at: 100,
        author: "x".into(),
    });
    pairs.push(("shared".into(), f1));
    pairs.push(("shared".into(), f2));
    db.replace_commit_files(&commits, &pairs).unwrap();

    let partners = db.cochange_for_file(f1, 0.5).unwrap();
    assert!(
        partners.is_empty(),
        "low-jaccard pair should be excluded at threshold 0.5"
    );

    let partners_low = db.cochange_for_file(f1, 0.01).unwrap();
    assert!(
        !partners_low.is_empty(),
        "pair should appear with a low enough threshold"
    );
}

#[test]
fn cochange_for_file_sorted_by_jaccard_desc() {
    let (_dir, db) = setup_db();

    let f1 = db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    let f2 = db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();
    let f3 = db.upsert_file("src/c.rs", "rust", "h3", 10, true).unwrap();

    let commits = vec![
        CommitRow {
            hash: "c1".into(),
            committed_at: 1000,
            author: "a@b.c".into(),
        },
        CommitRow {
            hash: "c2".into(),
            committed_at: 1001,
            author: "a@b.c".into(),
        },
        CommitRow {
            hash: "c3".into(),
            committed_at: 1002,
            author: "a@b.c".into(),
        },
    ];
    let pairs = vec![
        ("c1".into(), f1),
        ("c1".into(), f2),
        ("c1".into(), f3),
        ("c2".into(), f1),
        ("c2".into(), f2),
        ("c3".into(), f1),
    ];
    db.replace_commit_files(&commits, &pairs).unwrap();

    let partners = db.cochange_for_file(f1, 0.1).unwrap();
    for window in partners.windows(2) {
        assert!(
            window[0].1 >= window[1].1,
            "not sorted by jaccard: {} ({}) before {} ({})",
            window[0].0,
            window[0].1,
            window[1].0,
            window[1].1,
        );
    }
}

#[test]
fn jaccard_computation() {
    let (_dir, db) = setup_db();

    let f1 = db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    let f2 = db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();
    let f3 = db.upsert_file("src/c.rs", "rust", "h3", 10, true).unwrap();

    // f1 in c1-c5 (5 commits), f2 in c1-c4 (4 commits), shared = c1-c4 = 4
    // Jaccard = 4 / (5 + 4 - 4) = 4/5 = 0.8
    let commits = vec![
        CommitRow {
            hash: "c1".into(),
            committed_at: 1000,
            author: "a@b.c".into(),
        },
        CommitRow {
            hash: "c2".into(),
            committed_at: 1001,
            author: "a@b.c".into(),
        },
        CommitRow {
            hash: "c3".into(),
            committed_at: 1002,
            author: "a@b.c".into(),
        },
        CommitRow {
            hash: "c4".into(),
            committed_at: 1003,
            author: "a@b.c".into(),
        },
        CommitRow {
            hash: "c5".into(),
            committed_at: 1004,
            author: "a@b.c".into(),
        },
    ];
    let pairs = vec![
        ("c1".into(), f1),
        ("c2".into(), f1),
        ("c3".into(), f1),
        ("c4".into(), f1),
        ("c5".into(), f1),
        ("c1".into(), f2),
        ("c2".into(), f2),
        ("c3".into(), f2),
        ("c4".into(), f2),
        ("c1".into(), f3),
    ];
    db.replace_commit_files(&commits, &pairs).unwrap();

    let results = db.cochange_pairs_above_threshold(0.5).unwrap();
    assert!(!results.is_empty(), "should find pairs above threshold");

    let f1_f2 = results
        .iter()
        .find(|(a, b, _, _)| (*a == f1 && *b == f2) || (*a == f2 && *b == f1));
    assert!(f1_f2.is_some(), "f1-f2 pair should be found");
    let (_, _, jaccard, shared) = f1_f2.unwrap();
    assert!(
        (*jaccard - 0.8).abs() < 0.01,
        "jaccard should be ~0.8, got {jaccard}"
    );
    assert_eq!(*shared, 4);
}

#[test]
fn jaccard_below_threshold_excluded() {
    let (_dir, db) = setup_db();

    let f1 = db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    let f2 = db.upsert_file("src/b.rs", "rust", "h2", 10, true).unwrap();

    // f1 has 10 commits, f2 has 10 commits, they share 1
    // Jaccard = 1 / (10 + 10 - 1) = 1/19 ≈ 0.053
    let mut commits = Vec::new();
    let mut pairs = Vec::new();
    for i in 0..10 {
        let hash = format!("a{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: i,
            author: "x".into(),
        });
        pairs.push((hash, f1));
    }
    for i in 0..10 {
        let hash = format!("b{i}");
        commits.push(CommitRow {
            hash: hash.clone(),
            committed_at: i,
            author: "x".into(),
        });
        pairs.push((hash, f2));
    }
    // One shared commit
    commits.push(CommitRow {
        hash: "shared".into(),
        committed_at: 100,
        author: "x".into(),
    });
    pairs.push(("shared".into(), f1));
    pairs.push(("shared".into(), f2));

    db.replace_commit_files(&commits, &pairs).unwrap();

    let results = db.cochange_pairs_above_threshold(0.5).unwrap();
    assert!(
        results.is_empty(),
        "low-jaccard pair should be excluded at threshold 0.5"
    );
}

#[test]
fn commit_file_count_tracks_rows() {
    let (_dir, db) = setup_db();

    assert_eq!(db.commit_file_count().unwrap(), 0);

    let f1 = db.upsert_file("src/a.rs", "rust", "h1", 10, true).unwrap();
    let commits = vec![CommitRow {
        hash: "c1".into(),
        committed_at: 1000,
        author: "a@b.c".into(),
    }];
    let pairs = vec![("c1".into(), f1)];
    db.replace_commit_files(&commits, &pairs).unwrap();

    assert_eq!(db.commit_file_count().unwrap(), 1);
}

// --- entity-level co-change tests ---

fn make_change(name: &str, kind: &str, file: &str, change_type: &str) -> EntityChangeRow {
    EntityChangeRow {
        qualified_name: name.into(),
        kind: kind.into(),
        file_path: file.into(),
        change_type: change_type.into(),
        old_qualified_name: None,
        old_file_path: None,
    }
}

#[test]
fn entity_cochange_two_functions_across_commits() {
    let (_dir, db) = setup_db();

    // func_a and func_b co-edited in c1, c2, c3
    // func_a also edited alone in c4
    for (hash, ts) in [("c1", 100), ("c2", 200), ("c3", 300)] {
        db.insert_entity_commit_with_changes(
            hash,
            ts,
            "dev@test",
            &[
                make_change("func_a", "function", "src/a.rs", "body_changed"),
                make_change("func_b", "function", "src/b.rs", "body_changed"),
            ],
        )
        .unwrap();
    }
    db.insert_entity_commit_with_changes(
        "c4",
        400,
        "dev@test",
        &[make_change(
            "func_a",
            "function",
            "src/a.rs",
            "body_changed",
        )],
    )
    .unwrap();

    let partners = db.entity_cochange_for_symbol("func_a", 0.1).unwrap();
    assert_eq!(partners.len(), 1, "should find func_b");

    let (name, file, jaccard, confidence, shared) = &partners[0];
    assert_eq!(name, "func_b");
    assert_eq!(file, "src/b.rs");
    assert_eq!(*shared, 3);
    // jaccard = 3 / (4 + 3 - 3) = 3/4 = 0.75
    assert!(
        (*jaccard - 0.75).abs() < 0.01,
        "jaccard should be ~0.75, got {jaccard}"
    );
    // confidence = 3 / min(4, 3) = 3/3 = 1.0
    assert!(
        (*confidence - 1.0).abs() < 0.01,
        "confidence should be ~1.0, got {confidence}"
    );
}

#[test]
fn entity_cochange_bulk_commit_excluded_from_pairs() {
    let (_dir, db) = setup_db();

    // Create a commit with >50 entities — should be pair_ineligible
    let mut bulk_changes: Vec<EntityChangeRow> = (0..60)
        .map(|i| {
            make_change(
                &format!("sym_{i}"),
                "function",
                "src/big.rs",
                "body_changed",
            )
        })
        .collect();
    // Include func_a and func_b in the bulk commit
    bulk_changes.push(make_change(
        "func_a",
        "function",
        "src/a.rs",
        "body_changed",
    ));
    bulk_changes.push(make_change(
        "func_b",
        "function",
        "src/b.rs",
        "body_changed",
    ));
    db.insert_entity_commit_with_changes("bulk", 100, "dev@test", &bulk_changes)
        .unwrap();

    // A small commit with both func_a and func_b (pair_eligible)
    db.insert_entity_commit_with_changes(
        "small1",
        200,
        "dev@test",
        &[
            make_change("func_a", "function", "src/a.rs", "body_changed"),
            make_change("func_b", "function", "src/b.rs", "body_changed"),
        ],
    )
    .unwrap();

    // Another small commit
    db.insert_entity_commit_with_changes(
        "small2",
        300,
        "dev@test",
        &[
            make_change("func_a", "function", "src/a.rs", "body_changed"),
            make_change("func_b", "function", "src/b.rs", "body_changed"),
        ],
    )
    .unwrap();

    let partners = db.entity_cochange_for_symbol("func_a", 0.1).unwrap();
    let entry = partners.iter().find(|(n, _, _, _, _)| n == "func_b");
    assert!(entry.is_some(), "func_b should be a partner");

    // Only 2 shared commits (bulk excluded), not 3
    let (_, _, _, _, shared) = entry.unwrap();
    assert_eq!(*shared, 2, "bulk commit should be excluded from pair count");
}

#[test]
fn entity_cochange_rename_continuity() {
    let (_dir, db) = setup_db();

    // c1: old_name and partner co-edited
    db.insert_entity_commit_with_changes(
        "c1",
        100,
        "dev@test",
        &[
            make_change("old_name", "function", "src/a.rs", "body_changed"),
            make_change("partner", "function", "src/b.rs", "body_changed"),
        ],
    )
    .unwrap();

    // c2: old_name renamed to new_name, partner also edited
    let mut rename_change = make_change("new_name", "function", "src/a.rs", "renamed");
    rename_change.old_qualified_name = Some("old_name".into());
    db.insert_entity_commit_with_changes(
        "c2",
        200,
        "dev@test",
        &[
            rename_change,
            make_change("partner", "function", "src/b.rs", "body_changed"),
        ],
    )
    .unwrap();

    // c3: new_name and partner co-edited
    db.insert_entity_commit_with_changes(
        "c3",
        300,
        "dev@test",
        &[
            make_change("new_name", "function", "src/a.rs", "body_changed"),
            make_change("partner", "function", "src/b.rs", "body_changed"),
        ],
    )
    .unwrap();

    // Querying by new_name should find partner with shared=3 (includes pre-rename history)
    let partners = db.entity_cochange_for_symbol("new_name", 0.1).unwrap();
    let entry = partners.iter().find(|(n, _, _, _, _)| n == "partner");
    assert!(
        entry.is_some(),
        "partner should appear via rename continuity"
    );

    let (_, _, _, _, shared) = entry.unwrap();
    assert!(
        *shared >= 2,
        "should include pre-rename co-changes, got {shared}"
    );
}

#[test]
fn entity_cochange_merge_commit_no_changes() {
    let (_dir, db) = setup_db();

    // Merge commit — empty changes
    db.insert_entity_commit_with_changes("merge1", 100, "dev@test", &[])
        .unwrap();

    assert_eq!(db.entity_commit_count().unwrap(), 1);
    assert_eq!(db.entity_change_count().unwrap(), 0);
}

#[test]
fn entity_cochange_idempotent_insert() {
    let (_dir, db) = setup_db();

    db.insert_entity_commit_with_changes(
        "c1",
        100,
        "dev@test",
        &[make_change(
            "func_a",
            "function",
            "src/a.rs",
            "body_changed",
        )],
    )
    .unwrap();

    // Re-insert same hash — should be a no-op (INSERT OR IGNORE)
    db.insert_entity_commit_with_changes(
        "c1",
        100,
        "dev@test",
        &[make_change(
            "func_a",
            "function",
            "src/a.rs",
            "body_changed",
        )],
    )
    .unwrap();

    assert_eq!(db.entity_commit_count().unwrap(), 1);
    assert_eq!(
        db.entity_change_count().unwrap(),
        1,
        "re-insert should not create duplicate entity_changes"
    );
}

#[test]
fn entity_cochange_requires_min_shared() {
    let (_dir, db) = setup_db();

    // Only 1 shared commit — should be excluded (shared_cnt >= 2 filter)
    db.insert_entity_commit_with_changes(
        "c1",
        100,
        "dev@test",
        &[
            make_change("func_a", "function", "src/a.rs", "body_changed"),
            make_change("func_b", "function", "src/b.rs", "body_changed"),
        ],
    )
    .unwrap();

    let partners = db.entity_cochange_for_symbol("func_a", 0.0).unwrap();
    assert!(
        partners.is_empty(),
        "single shared commit should not produce a pair"
    );
}

#[test]
fn known_entity_commit_hashes_returns_indexed() {
    let (_dir, db) = setup_db();

    db.insert_entity_commit_with_changes("abc123", 100, "dev@test", &[])
        .unwrap();
    db.insert_entity_commit_with_changes("def456", 200, "dev@test", &[])
        .unwrap();

    let known = db.known_entity_commit_hashes().unwrap();
    assert!(known.contains("abc123"));
    assert!(known.contains("def456"));
    assert_eq!(known.len(), 2);
}
