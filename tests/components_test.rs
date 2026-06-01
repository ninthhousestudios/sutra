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

// ---------------------------------------------------------------------------
// Reconciliation tests
// ---------------------------------------------------------------------------

fn setup_two_clusters(db: &sutra::db::Db) {
    let a1 = db.upsert_file("src/core/a1.rs", "rust", "h1", 50, true).unwrap();
    let a2 = db.upsert_file("src/core/a2.rs", "rust", "h2", 50, true).unwrap();
    let a3 = db.upsert_file("src/core/a3.rs", "rust", "h3", 50, true).unwrap();
    let b1 = db.upsert_file("src/tools/b1.rs", "rust", "h4", 50, true).unwrap();
    let b2 = db.upsert_file("src/tools/b2.rs", "rust", "h5", 50, true).unwrap();
    let b3 = db.upsert_file("src/tools/b3.rs", "rust", "h6", 50, true).unwrap();

    let sa1 = insert_symbol(&db, a1, "core_a1");
    let sa2 = insert_symbol(&db, a2, "core_a2");
    let sa3 = insert_symbol(&db, a3, "core_a3");
    let sb1 = insert_symbol(&db, b1, "tools_b1");
    let sb2 = insert_symbol(&db, b2, "tools_b2");
    let sb3 = insert_symbol(&db, b3, "tools_b3");

    // Dense intra-cluster refs
    insert_refs(&db, a1, sa2, 10);
    insert_refs(&db, a1, sa3, 10);
    insert_refs(&db, a2, sa1, 10);
    insert_refs(&db, a2, sa3, 10);
    insert_refs(&db, a3, sa1, 10);
    insert_refs(&db, a3, sa2, 10);

    insert_refs(&db, b1, sb2, 10);
    insert_refs(&db, b1, sb3, 10);
    insert_refs(&db, b2, sb1, 10);
    insert_refs(&db, b2, sb3, 10);
    insert_refs(&db, b3, sb1, 10);
    insert_refs(&db, b3, sb2, 10);
}

#[test]
fn test_reconciliation_preserves_component_identity() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path()).unwrap();
    assert_eq!(count, 2);

    let original_comps = db.all_components().unwrap();
    let original_ids: std::collections::HashSet<String> =
        original_comps.iter().map(|c| c.id.clone()).collect();

    // Reindex wipes ephemeral tables
    db.reindex().unwrap();
    assert!(db.component_count().unwrap() > 0);
    assert_eq!(db.membership_count().unwrap(), 0);

    // Re-insert identical files and refs
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path()).unwrap();
    assert_eq!(count, 2, "reconciliation should produce 2 components");

    let reconciled_comps = db.all_components().unwrap();
    let reconciled_ids: std::collections::HashSet<String> =
        reconciled_comps.iter().map(|c| c.id.clone()).collect();

    assert_eq!(original_ids, reconciled_ids, "component IDs should be preserved");

    // Membership should be repopulated
    for c in &reconciled_comps {
        let paths = db.component_file_paths(&c.id).unwrap();
        assert_eq!(paths.len(), 3, "component '{}' should have 3 files", c.name);
    }
}

fn pin_resolution(dir: &std::path::Path) {
    let sutra_dir = dir.join(".sutra");
    std::fs::create_dir_all(&sutra_dir).unwrap();
    std::fs::write(sutra_dir.join("components.toml"), "resolution = 1.0").unwrap();
}

#[test]
fn test_dissolved_components_hidden_from_queries() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();
    assert_eq!(db.all_components().unwrap().len(), 2);

    // Reindex and only re-insert one cluster (tools files removed)
    db.reindex().unwrap();
    let a1 = db.upsert_file("src/core/a1.rs", "rust", "h1", 50, true).unwrap();
    let a2 = db.upsert_file("src/core/a2.rs", "rust", "h2", 50, true).unwrap();
    let a3 = db.upsert_file("src/core/a3.rs", "rust", "h3", 50, true).unwrap();
    let sa1 = insert_symbol(&db, a1, "core_a1");
    let sa2 = insert_symbol(&db, a2, "core_a2");
    let sa3 = insert_symbol(&db, a3, "core_a3");
    insert_refs(&db, a1, sa2, 10);
    insert_refs(&db, a1, sa3, 10);
    insert_refs(&db, a2, sa1, 10);
    insert_refs(&db, a2, sa3, 10);
    insert_refs(&db, a3, sa1, 10);
    insert_refs(&db, a3, sa2, 10);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();

    // Only 1 active component visible
    let active = db.all_components().unwrap();
    assert_eq!(active.len(), 1, "dissolved component should be hidden");
    assert_eq!(active[0].name, "core");

    // MCP tool should also return only 1
    let result = sutra::tools::components::handle(&db).unwrap();
    assert_eq!(result["total"].as_u64().unwrap(), 1);
}

#[test]
fn test_merge_event_detected() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();
    let original = db.all_components().unwrap();
    assert_eq!(original.len(), 2);

    // Reindex and re-insert all files, but now with cross-cluster refs
    // so Louvain merges everything into 1 cluster
    db.reindex().unwrap();

    let a1 = db.upsert_file("src/core/a1.rs", "rust", "h1", 50, true).unwrap();
    let a2 = db.upsert_file("src/core/a2.rs", "rust", "h2", 50, true).unwrap();
    let a3 = db.upsert_file("src/core/a3.rs", "rust", "h3", 50, true).unwrap();
    let b1 = db.upsert_file("src/tools/b1.rs", "rust", "h4", 50, true).unwrap();
    let b2 = db.upsert_file("src/tools/b2.rs", "rust", "h5", 50, true).unwrap();
    let b3 = db.upsert_file("src/tools/b3.rs", "rust", "h6", 50, true).unwrap();

    let sa1 = insert_symbol(&db, a1, "core_a1");
    let sa2 = insert_symbol(&db, a2, "core_a2");
    let sa3 = insert_symbol(&db, a3, "core_a3");
    let sb1 = insert_symbol(&db, b1, "tools_b1");
    let sb2 = insert_symbol(&db, b2, "tools_b2");
    let sb3 = insert_symbol(&db, b3, "tools_b3");

    // Dense refs across ALL files (merged clique)
    for &(from, to_sym) in &[
        (a1, sa2), (a1, sa3), (a1, sb1), (a1, sb2), (a1, sb3),
        (a2, sa1), (a2, sa3), (a2, sb1), (a2, sb2), (a2, sb3),
        (a3, sa1), (a3, sa2), (a3, sb1), (a3, sb2), (a3, sb3),
        (b1, sa1), (b1, sa2), (b1, sa3), (b1, sb2), (b1, sb3),
        (b2, sa1), (b2, sa2), (b2, sa3), (b2, sb1), (b2, sb3),
        (b3, sa1), (b3, sa2), (b3, sa3), (b3, sb1), (b3, sb2),
    ] {
        insert_refs(&db, from, to_sym, 10);
    }

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();

    let active = db.all_components().unwrap();
    assert_eq!(active.len(), 1, "should have 1 component after merge");

    // The surviving component should have a merge event
    let events = db.component_events(&active[0].id).unwrap();
    let merge_events: Vec<_> = events.iter().filter(|(t, _)| t == "merge").collect();
    assert_eq!(merge_events.len(), 1, "should have exactly 1 merge event");

    // The absorbed component should be identified in the event detail
    let detail: serde_json::Value = serde_json::from_str(&merge_events[0].1).unwrap();
    let absorbed = detail["absorbed"].as_array().unwrap();
    assert_eq!(absorbed.len(), 2, "both prior components were absorbed");
    for entry in absorbed {
        assert!(entry["id"].is_string());
        assert!(entry["name"].is_string());
    }
}

#[test]
fn test_split_event_detected() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());

    // Start with 1 big cluster: 6 files all cross-referenced
    let a1 = db.upsert_file("src/core/a1.rs", "rust", "h1", 50, true).unwrap();
    let a2 = db.upsert_file("src/core/a2.rs", "rust", "h2", 50, true).unwrap();
    let a3 = db.upsert_file("src/core/a3.rs", "rust", "h3", 50, true).unwrap();
    let b1 = db.upsert_file("src/tools/b1.rs", "rust", "h4", 50, true).unwrap();
    let b2 = db.upsert_file("src/tools/b2.rs", "rust", "h5", 50, true).unwrap();
    let b3 = db.upsert_file("src/tools/b3.rs", "rust", "h6", 50, true).unwrap();

    let sa1 = insert_symbol(&db, a1, "core_a1");
    let sa2 = insert_symbol(&db, a2, "core_a2");
    let sa3 = insert_symbol(&db, a3, "core_a3");
    let sb1 = insert_symbol(&db, b1, "tools_b1");
    let sb2 = insert_symbol(&db, b2, "tools_b2");
    let sb3 = insert_symbol(&db, b3, "tools_b3");

    // Dense refs across ALL files
    for &(from, to_sym) in &[
        (a1, sa2), (a1, sa3), (a1, sb1), (a1, sb2), (a1, sb3),
        (a2, sa1), (a2, sa3), (a2, sb1), (a2, sb2), (a2, sb3),
        (a3, sa1), (a3, sa2), (a3, sb1), (a3, sb2), (a3, sb3),
        (b1, sa1), (b1, sa2), (b1, sa3), (b1, sb2), (b1, sb3),
        (b2, sa1), (b2, sa2), (b2, sa3), (b2, sb1), (b2, sb3),
        (b3, sa1), (b3, sa2), (b3, sa3), (b3, sb1), (b3, sb2),
    ] {
        insert_refs(&db, from, to_sym, 10);
    }

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path()).unwrap();
    assert_eq!(count, 1, "should be 1 big component");

    let original = db.all_components().unwrap();
    assert_eq!(original.len(), 1);
    let original_id = original[0].id.clone();

    // Reindex and split into 2 separate cliques (no cross-cluster refs)
    db.reindex().unwrap();
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();

    // Original component should match one cluster (>60% overlap)
    // and split event should fire because its files span 2 clusters
    let active = db.all_components().unwrap();
    assert!(active.len() >= 2, "should have at least 2 components after split");

    // Find events on the original component (if it survived)
    let events = db.component_events(&original_id).unwrap();
    let split_events: Vec<_> = events.iter().filter(|(t, _)| t == "split").collect();
    assert_eq!(split_events.len(), 1, "should have exactly 1 split event");

    let detail: serde_json::Value = serde_json::from_str(&split_events[0].1).unwrap();
    assert!(detail["clusters"].as_u64().unwrap() >= 2);
}

#[test]
fn test_drift_event_detected() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());

    // 2 clusters: A has 5 files, B has 5 files
    let a_files: Vec<i64> = (1..=5)
        .map(|i| {
            db.upsert_file(
                &format!("src/core/a{}.rs", i), "rust",
                &format!("ha{}", i), 50, true,
            ).unwrap()
        })
        .collect();
    let b_files: Vec<i64> = (1..=5)
        .map(|i| {
            db.upsert_file(
                &format!("src/tools/b{}.rs", i), "rust",
                &format!("hb{}", i), 50, true,
            ).unwrap()
        })
        .collect();

    // Symbols for each file
    let a_syms: Vec<i64> = a_files.iter().enumerate()
        .map(|(i, &fid)| insert_symbol(&db, fid, &format!("core_a{}", i + 1)))
        .collect();
    let b_syms: Vec<i64> = b_files.iter().enumerate()
        .map(|(i, &fid)| insert_symbol(&db, fid, &format!("tools_b{}", i + 1)))
        .collect();

    // Dense intra-cluster refs
    for i in 0..5 {
        for j in 0..5 {
            if i != j {
                insert_refs(&db, a_files[i], a_syms[j], 10);
                insert_refs(&db, b_files[i], b_syms[j], 10);
            }
        }
    }

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();
    let original = db.all_components().unwrap();
    assert_eq!(original.len(), 2);

    let comp_a = original.iter().find(|c| c.name == "core").unwrap();
    let comp_a_id = comp_a.id.clone();

    // Reindex and move 2 of A's files (40%) to B's cluster
    db.reindex().unwrap();

    // A keeps 3 files
    let a_files2: Vec<i64> = (1..=3)
        .map(|i| {
            db.upsert_file(
                &format!("src/core/a{}.rs", i), "rust",
                &format!("ha{}", i), 50, true,
            ).unwrap()
        })
        .collect();
    // B gets its 5 files + 2 from A
    let b_files2: Vec<i64> = (1..=5)
        .map(|i| {
            db.upsert_file(
                &format!("src/tools/b{}.rs", i), "rust",
                &format!("hb{}", i), 50, true,
            ).unwrap()
        })
        .collect();
    let drift_files: Vec<i64> = (4..=5)
        .map(|i| {
            db.upsert_file(
                &format!("src/core/a{}.rs", i), "rust",
                &format!("ha{}", i), 50, true,
            ).unwrap()
        })
        .collect();

    // Symbols
    let a_syms2: Vec<i64> = a_files2.iter().enumerate()
        .map(|(i, &fid)| insert_symbol(&db, fid, &format!("core_a{}", i + 1)))
        .collect();
    let b_syms2: Vec<i64> = b_files2.iter().enumerate()
        .map(|(i, &fid)| insert_symbol(&db, fid, &format!("tools_b{}", i + 1)))
        .collect();
    let drift_syms: Vec<i64> = drift_files.iter().enumerate()
        .map(|(i, &fid)| insert_symbol(&db, fid, &format!("core_a{}", i + 4)))
        .collect();

    // Dense intra-cluster refs for A (3 files)
    for i in 0..3 {
        for j in 0..3 {
            if i != j { insert_refs(&db, a_files2[i], a_syms2[j], 10); }
        }
    }
    // Dense intra-cluster refs for B (5 files + 2 drifted files)
    let all_b = [b_files2.as_slice(), drift_files.as_slice()].concat();
    let all_b_syms = [b_syms2.as_slice(), drift_syms.as_slice()].concat();
    for i in 0..all_b.len() {
        for j in 0..all_b.len() {
            if i != j { insert_refs(&db, all_b[i], all_b_syms[j], 10); }
        }
    }

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();

    // Component A should have a drift event (40% of its files moved to B)
    let events = db.component_events(&comp_a_id).unwrap();
    let drift_events: Vec<_> = events.iter().filter(|(t, _)| t == "drift").collect();
    assert_eq!(drift_events.len(), 1, "should have exactly 1 drift event on comp A");

    let detail: serde_json::Value = serde_json::from_str(&drift_events[0].1).unwrap();
    let ratio = detail["ratio"].as_f64().unwrap();
    assert!(ratio > 0.3, "drift ratio should be > 0.3, got {}", ratio);
    assert_eq!(detail["shifted_files"].as_u64().unwrap(), 2);
}

#[test]
fn test_unmatched_cluster_creates_new_component() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());

    // Start with 1 cluster (core: a1,a2,a3)
    let a1 = db.upsert_file("src/core/a1.rs", "rust", "h1", 50, true).unwrap();
    let a2 = db.upsert_file("src/core/a2.rs", "rust", "h2", 50, true).unwrap();
    let a3 = db.upsert_file("src/core/a3.rs", "rust", "h3", 50, true).unwrap();
    let sa1 = insert_symbol(&db, a1, "core_a1");
    let sa2 = insert_symbol(&db, a2, "core_a2");
    let sa3 = insert_symbol(&db, a3, "core_a3");
    insert_refs(&db, a1, sa2, 10);
    insert_refs(&db, a1, sa3, 10);
    insert_refs(&db, a2, sa1, 10);
    insert_refs(&db, a2, sa3, 10);
    insert_refs(&db, a3, sa1, 10);
    insert_refs(&db, a3, sa2, 10);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();
    let original = db.all_components().unwrap();
    assert_eq!(original.len(), 1);
    let original_id = original[0].id.clone();

    // Reindex and add a second cluster (tools: b1,b2,b3) alongside the original
    db.reindex().unwrap();
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path()).unwrap();

    let active = db.all_components().unwrap();
    assert_eq!(active.len(), 2, "original preserved + new component created");

    // Original component should still exist
    assert!(
        active.iter().any(|c| c.id == original_id),
        "original component ID should be preserved"
    );

    // New component should have "tools" name
    let new_comp = active.iter().find(|c| c.id != original_id).unwrap();
    assert_eq!(new_comp.name, "tools");
}
