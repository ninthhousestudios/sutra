use std::path::Path;

use sutra::db::{CommitRow, Db};
use sutra::git::{git_cochange_files, git_commit_files};

fn sutra_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

#[test]
fn cochange_returns_ok_for_known_file() {
    let result = git_cochange_files(sutra_root(), "src/git.rs", 90);
    assert!(result.is_ok(), "git_cochange_files failed: {result:?}");
}

#[test]
fn cochange_result_is_sorted_descending() {
    let pairs = git_cochange_files(sutra_root(), "src/mcp.rs", 180).unwrap();
    for window in pairs.windows(2) {
        assert!(
            window[0].1 >= window[1].1,
            "not sorted: {} ({}) before {} ({})",
            window[0].0,
            window[0].1,
            window[1].0,
            window[1].1,
        );
    }
}

#[test]
fn cochange_excludes_queried_file() {
    let pairs = git_cochange_files(sutra_root(), "src/git.rs", 180).unwrap();
    assert!(
        !pairs.iter().any(|(p, _)| p == "src/git.rs"),
        "queried file should be excluded from results"
    );
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
