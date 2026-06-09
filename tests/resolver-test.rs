use std::collections::HashMap;

use sutra::constraints::ConstraintResolver;
use sutra::db::Db;
use sutra::rules::{Constraint, ConstraintKind, Severity};

fn setup_db() -> (Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (db, dir)
}

fn make_constraint(kind: ConstraintKind) -> Constraint {
    make_constraint_with_id("test0000", kind)
}

fn make_constraint_with_id(id: &str, kind: ConstraintKind) -> Constraint {
    Constraint {
        id: id.into(),
        kind,
        severity: Severity::Blocking,
        name: None,
        provenance: None,
        scope: None,
    }
}

fn setup_files(db: &Db, paths: &[&str]) -> HashMap<i64, String> {
    let mut map = HashMap::new();
    for &p in paths {
        let id = db.upsert_file(p, "rust", "hash", 50, true).unwrap();
        map.insert(id, p.to_string());
    }
    map
}

fn setup_component(db: &Db, name: &str, file_ids: &[i64]) -> String {
    let cid = format!("comp-{name}");
    let prior_paths = String::new();
    db.batch_create_components(
        &[(cid.clone(), name.to_string(), prior_paths)],
        &file_ids
            .iter()
            .map(|&fid| (cid.clone(), fid))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    cid
}

#[test]
fn boundary_resolution_with_membership() {
    let (db, _dir) = setup_db();
    let path_map = setup_files(
        &db,
        &[
            "src/core/a.rs",
            "src/core/b.rs",
            "src/tools/x.rs",
            "src/tools/y.rs",
        ],
    );

    let core_ids: Vec<i64> = path_map
        .iter()
        .filter(|(_, p)| p.starts_with("src/core/"))
        .map(|(&id, _)| id)
        .collect();
    let tool_ids: Vec<i64> = path_map
        .iter()
        .filter(|(_, p)| p.starts_with("src/tools/"))
        .map(|(&id, _)| id)
        .collect();

    setup_component(&db, "core", &core_ids);
    setup_component(&db, "tools", &tool_ids);
    db.upsert_clustering_meta(0, 4, "h1", 0, 0).unwrap();

    let constraint = make_constraint(ConstraintKind::Boundary {
        from_component: "tools".into(),
        to_component: "core".into(),
    });

    let mut resolver = ConstraintResolver::new();
    let pairs = resolver.resolve(&[constraint], &db, &path_map).unwrap();

    // Cross product: 2 tool files × 2 core files = 4 pairs
    assert_eq!(pairs.len(), 4);
    for &tid in &tool_ids {
        for &cid in &core_ids {
            assert!(pairs.contains(&(tid, cid)), "missing ({tid}, {cid})");
        }
    }
}

#[test]
fn cache_hit_on_unchanged_membership() {
    let (db, _dir) = setup_db();
    let path_map = setup_files(&db, &["src/a.rs", "src/b.rs"]);
    let ids: Vec<i64> = path_map.keys().copied().collect();

    setup_component(&db, "alpha", &ids[..1]);
    setup_component(&db, "beta", &ids[1..]);
    db.upsert_clustering_meta(0, 2, "h1", 0, 0).unwrap();

    let constraint = make_constraint(ConstraintKind::Boundary {
        from_component: "alpha".into(),
        to_component: "beta".into(),
    });

    let mut resolver = ConstraintResolver::new();
    let first = resolver
        .resolve(&[constraint.clone()], &db, &path_map)
        .unwrap();
    let second = resolver.resolve(&[constraint], &db, &path_map).unwrap();
    assert_eq!(first, second);
}

#[test]
fn cache_miss_on_membership_change() {
    let (db, _dir) = setup_db();
    let path_map = setup_files(&db, &["src/a.rs", "src/b.rs", "src/c.rs"]);
    let mut ids: Vec<i64> = path_map.keys().copied().collect();
    ids.sort();

    setup_component(&db, "alpha", &ids[..1]);
    setup_component(&db, "beta", &ids[1..2]);
    db.upsert_clustering_meta(0, 3, "h1", 0, 0).unwrap();

    let constraint = make_constraint(ConstraintKind::Boundary {
        from_component: "alpha".into(),
        to_component: "beta".into(),
    });

    let mut resolver = ConstraintResolver::new();
    let first = resolver
        .resolve(&[constraint.clone()], &db, &path_map)
        .unwrap();
    assert_eq!(first.len(), 1);

    // Simulate membership change: add third file to beta, update clustering_meta
    db.batch_insert_membership(&[("comp-beta".into(), ids[2])])
        .unwrap();
    db.upsert_clustering_meta(0, 3, "h2", 0, 0).unwrap();

    let second = resolver.resolve(&[constraint], &db, &path_map).unwrap();
    assert_eq!(second.len(), 2);
}

#[test]
fn forbidden_dep_passes_through() {
    let (db, _dir) = setup_db();
    let path_map = setup_files(
        &db,
        &[
            "src/tools/review.rs",
            "src/tools/orient.rs",
            "src/daemon.rs",
        ],
    );

    let constraint = make_constraint(ConstraintKind::ForbiddenDep {
        from: "src/tools/*".into(),
        to: "src/daemon.rs".into(),
    });

    let daemon_id = *path_map
        .iter()
        .find(|(_, p)| p.as_str() == "src/daemon.rs")
        .unwrap()
        .0;
    let tool_ids: Vec<i64> = path_map
        .iter()
        .filter(|(_, p)| p.starts_with("src/tools/"))
        .map(|(&id, _)| id)
        .collect();

    let mut resolver = ConstraintResolver::new();
    let pairs = resolver.resolve(&[constraint], &db, &path_map).unwrap();

    assert_eq!(pairs.len(), 2);
    for &tid in &tool_ids {
        assert!(pairs.contains(&(tid, daemon_id)));
    }
}

#[test]
fn mixed_boundary_and_forbidden_dep() {
    let (db, _dir) = setup_db();
    let path_map = setup_files(&db, &["src/core/a.rs", "src/tools/x.rs", "src/daemon.rs"]);

    let core_id = *path_map
        .iter()
        .find(|(_, p)| p.as_str() == "src/core/a.rs")
        .unwrap()
        .0;
    let tool_id = *path_map
        .iter()
        .find(|(_, p)| p.as_str() == "src/tools/x.rs")
        .unwrap()
        .0;
    let daemon_id = *path_map
        .iter()
        .find(|(_, p)| p.as_str() == "src/daemon.rs")
        .unwrap()
        .0;

    setup_component(&db, "core", &[core_id]);
    setup_component(&db, "tools", &[tool_id]);
    db.upsert_clustering_meta(0, 3, "h1", 0, 0).unwrap();

    let constraints = vec![
        make_constraint(ConstraintKind::Boundary {
            from_component: "tools".into(),
            to_component: "core".into(),
        }),
        make_constraint(ConstraintKind::ForbiddenDep {
            from: "src/tools/*".into(),
            to: "src/daemon.rs".into(),
        }),
    ];

    let mut resolver = ConstraintResolver::new();
    let pairs = resolver.resolve(&constraints, &db, &path_map).unwrap();

    assert!(pairs.contains(&(tool_id, core_id)));
    assert!(pairs.contains(&(tool_id, daemon_id)));
    assert_eq!(pairs.len(), 2);
}

#[test]
fn unknown_component_skipped() {
    let (db, _dir) = setup_db();
    let path_map = setup_files(&db, &["src/a.rs"]);

    let constraint = make_constraint(ConstraintKind::Boundary {
        from_component: "nonexistent".into(),
        to_component: "also-missing".into(),
    });

    let mut resolver = ConstraintResolver::new();
    let pairs = resolver.resolve(&[constraint], &db, &path_map).unwrap();
    assert!(pairs.is_empty());
}

#[test]
fn empty_constraints() {
    let (db, _dir) = setup_db();
    let path_map = setup_files(&db, &["src/a.rs"]);

    let mut resolver = ConstraintResolver::new();
    let pairs = resolver.resolve(&[], &db, &path_map).unwrap();
    assert!(pairs.is_empty());
}

#[test]
fn cache_miss_on_constraint_change() {
    let (db, _dir) = setup_db();
    let path_map = setup_files(
        &db,
        &[
            "src/tools/review.rs",
            "src/tools/orient.rs",
            "src/daemon.rs",
        ],
    );

    let c1 = make_constraint_with_id(
        "rule0001",
        ConstraintKind::ForbiddenDep {
            from: "src/tools/*".into(),
            to: "src/daemon.rs".into(),
        },
    );

    let mut resolver = ConstraintResolver::new();
    let first = resolver.resolve(&[c1], &db, &path_map).unwrap();
    assert_eq!(first.len(), 2);

    // Different constraint (different ID, different globs) — must re-resolve
    let c2 = make_constraint_with_id(
        "rule0002",
        ConstraintKind::ForbiddenDep {
            from: "src/daemon.rs".into(),
            to: "src/tools/*".into(),
        },
    );
    let second = resolver.resolve(&[c2], &db, &path_map).unwrap();
    assert_eq!(second.len(), 2);
    assert_ne!(
        first, second,
        "different constraints must produce different pairs"
    );
}

#[test]
fn cache_miss_on_path_map_change() {
    let (db, _dir) = setup_db();
    let mut path_map = setup_files(&db, &["src/tools/review.rs", "src/daemon.rs"]);

    let constraint = make_constraint(ConstraintKind::ForbiddenDep {
        from: "src/tools/*".into(),
        to: "src/daemon.rs".into(),
    });

    let mut resolver = ConstraintResolver::new();
    let first = resolver
        .resolve(&[constraint.clone()], &db, &path_map)
        .unwrap();
    assert_eq!(first.len(), 1);

    // Add a new file matching the glob — must re-resolve
    let new_id = db
        .upsert_file("src/tools/orient.rs", "rust", "hash2", 30, true)
        .unwrap();
    path_map.insert(new_id, "src/tools/orient.rs".to_string());

    let second = resolver.resolve(&[constraint], &db, &path_map).unwrap();
    assert_eq!(
        second.len(),
        2,
        "added file should produce an additional pair"
    );
}
