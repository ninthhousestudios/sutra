use sutra::components;
use sutra::db::{Db, InsertSymbolParams};

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

fn insert_symbol(db: &Db, file_id: i64, name: &str) -> i64 {
    db.insert_symbol(&InsertSymbolParams {
        file_id,
        qualified_name: name,
        short_name: name,
        kind: "function",
        signature: None,
        signature_hash: None,
        visibility: Some("public"),
        start_line: 1,
        start_col: 0,
        end_line: 10,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        flags: 0,
        language_attrs: None,
    })
    .unwrap()
}

fn insert_refs(db: &Db, from_file: i64, to_sym: i64, count: usize) {
    for i in 0..count {
        db.insert_ref(from_file, Some(to_sym), None, i as i64 + 1, 0, "call")
            .unwrap();
    }
}

#[test]
fn test_two_cluster_discovery() {
    let (dir, db) = setup_db();

    // Cluster A: 3 files under src/core/
    let a1 = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/a2.rs", "rust", "h2", 50, true)
        .unwrap();
    let a3 = db
        .upsert_file("src/core/a3.rs", "rust", "h3", 50, true)
        .unwrap();

    // Cluster B: 3 files under src/tools/
    let b1 = db
        .upsert_file("src/tools/b1.rs", "rust", "h4", 50, true)
        .unwrap();
    let b2 = db
        .upsert_file("src/tools/b2.rs", "rust", "h5", 50, true)
        .unwrap();
    let b3 = db
        .upsert_file("src/tools/b3.rs", "rust", "h6", 50, true)
        .unwrap();

    // Symbols — one per file
    let sa1 = insert_symbol(&db, a1, "core_a1_fn");
    let sa2 = insert_symbol(&db, a2, "core_a2_fn");
    let sa3 = insert_symbol(&db, a3, "core_a3_fn");
    let sb1 = insert_symbol(&db, b1, "tools_b1_fn");
    let sb2 = insert_symbol(&db, b2, "tools_b2_fn");
    let sb3 = insert_symbol(&db, b3, "tools_b3_fn");

    // Dense cross-refs within cluster A
    insert_refs(&db, a1, sa2, 10);
    insert_refs(&db, a1, sa3, 10);
    insert_refs(&db, a2, sa1, 10);
    insert_refs(&db, a2, sa3, 10);
    insert_refs(&db, a3, sa1, 10);
    insert_refs(&db, a3, sa2, 10);

    // Dense cross-refs within cluster B
    insert_refs(&db, b1, sb2, 10);
    insert_refs(&db, b1, sb3, 10);
    insert_refs(&db, b2, sb1, 10);
    insert_refs(&db, b2, sb3, 10);
    insert_refs(&db, b3, sb1, 10);
    insert_refs(&db, b3, sb2, 10);

    // No cross-cluster refs

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path()).unwrap();

    assert_eq!(count, 2, "should discover exactly 2 components");

    let comps = db.all_components().unwrap();
    assert_eq!(comps.len(), 2);

    // Each component should have 3 files
    for c in &comps {
        let paths = db.component_file_paths(&c.id).unwrap();
        assert_eq!(paths.len(), 3, "component '{}' should have 3 files", c.name);
    }

    // Names should be derived from path prefixes
    let mut names: Vec<&str> = comps.iter().map(|c| c.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["core", "tools"]);
}

#[test]
fn test_first_run_gate_skips_when_components_and_membership_exist() {
    let (dir, db) = setup_db();

    let a1 = db
        .upsert_file("src/a.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/b.rs", "rust", "h2", 50, true)
        .unwrap();
    let sa1 = insert_symbol(&db, a1, "fn_a");
    insert_refs(&db, a2, sa1, 5);

    // First run creates components + membership
    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path()).unwrap();
    assert!(count > 0);

    // Second run should skip — both components and membership exist
    let count2 = components::discover_components(&db, &files, dir.path()).unwrap();
    assert_eq!(count2, 0, "should skip when components and membership exist");
}

#[test]
fn test_orphan_cleanup_rediscovers_after_membership_wiped() {
    let (dir, db) = setup_db();

    let a1 = db
        .upsert_file("src/core/a.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/b.rs", "rust", "h2", 50, true)
        .unwrap();
    let sa1 = insert_symbol(&db, a1, "fn_a");
    insert_refs(&db, a2, sa1, 5);

    // First run
    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();
    let original_comps = db.all_components().unwrap();
    assert!(!original_comps.is_empty());

    // Simulate reindex wiping ephemeral membership
    db.reindex().unwrap();

    // Membership gone, components still there
    assert!(db.component_count().unwrap() > 0);
    assert_eq!(db.membership_count().unwrap(), 0);

    // Re-insert files and refs (reindex wiped them too)
    let a1 = db
        .upsert_file("src/core/a.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/b.rs", "rust", "h2", 50, true)
        .unwrap();
    let sa1 = insert_symbol(&db, a1, "fn_a");
    insert_refs(&db, a2, sa1, 5);

    // Re-discover should clean up orphans and create fresh components
    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path()).unwrap();
    assert!(count > 0, "should rediscover after orphan cleanup");

    // Membership should be populated again
    assert!(db.membership_count().unwrap() > 0);
    for c in &db.all_components().unwrap() {
        let paths = db.component_file_paths(&c.id).unwrap();
        assert!(!paths.is_empty(), "every component should have files");
    }
}

#[test]
fn test_no_edges_produces_no_components() {
    let (dir, db) = setup_db();
    db.upsert_file("src/a.rs", "rust", "h1", 50, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "h2", 50, true).unwrap();

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path()).unwrap();
    assert_eq!(count, 0, "no edges means nothing to cluster");
}

#[test]
fn test_config_override_resolution() {
    let (dir, _db) = setup_db();

    // Write config with explicit resolution
    let sutra_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&sutra_dir).unwrap();
    std::fs::write(sutra_dir.join("components.toml"), "resolution = 2.0").unwrap();

    let config = components::load_config(dir.path()).unwrap();
    assert_eq!(config.resolution, Some(2.0));
}

#[test]
fn test_components_get_stable_uuids() {
    let (dir, db) = setup_db();

    let a1 = db
        .upsert_file("src/core/a.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/b.rs", "rust", "h2", 50, true)
        .unwrap();
    let sa1 = insert_symbol(&db, a1, "fn_a");
    insert_refs(&db, a2, sa1, 5);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();

    let comps = db.all_components().unwrap();
    assert!(!comps.is_empty());
    for c in &comps {
        assert_eq!(c.id.len(), 36, "UUID should be 36 chars (hyphenated)");
        assert!(c.id.contains('-'), "UUID should contain hyphens");
    }
}

#[test]
fn test_sutra_components_tool() {
    let (dir, db) = setup_db();

    let a1 = db
        .upsert_file("src/core/a.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/b.rs", "rust", "h2", 50, true)
        .unwrap();
    let sa1 = insert_symbol(&db, a1, "fn_a");
    insert_refs(&db, a2, sa1, 5);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();

    let result = sutra::tools::components::handle(&db).unwrap();
    let obj = result.as_object().unwrap();
    let total = obj["total"].as_u64().unwrap();
    assert!(total > 0, "tool should return discovered components");

    let comps_arr = obj["components"].as_array().unwrap();
    for c in comps_arr {
        assert!(c["id"].is_string());
        assert!(c["name"].is_string());
        assert!(c["files"].is_array());
        assert!(c["file_count"].is_number());
    }
}
