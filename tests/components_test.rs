use sutra::components;
use sutra::db::{Db, InsertSymbolParams};

use std::collections::{HashMap, HashSet};

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

fn insert_symbol_with_kind(db: &Db, file_id: i64, qualified: &str, short: &str, kind: &str) -> i64 {
    db.insert_symbol(&InsertSymbolParams {
        file_id,
        qualified_name: qualified,
        short_name: short,
        kind,
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
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    })
    .unwrap()
}

fn insert_symbol(db: &Db, file_id: i64, name: &str) -> i64 {
    insert_symbol_with_kind(db, file_id, name, name, "function")
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
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

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

    let a1 = db.upsert_file("src/a.rs", "rust", "h1", 50, true).unwrap();
    let a2 = db.upsert_file("src/b.rs", "rust", "h2", 50, true).unwrap();
    let sa1 = insert_symbol(&db, a1, "fn_a");
    insert_refs(&db, a2, sa1, 5);

    // First run creates components + membership
    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert!(count > 0);

    // Second run should skip — both components and membership exist
    let count2 = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert_eq!(
        count2, 0,
        "should skip when components and membership exist"
    );
}

#[test]
fn test_reconciliation_after_reindex_preserves_and_repopulates() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());

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
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    let original_comps = db.all_components().unwrap();
    assert!(!original_comps.is_empty());
    let original_ids: std::collections::HashSet<String> =
        original_comps.iter().map(|c| c.id.clone()).collect();

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

    // Reconciliation should preserve component identity
    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert!(count > 0, "should reconcile after reindex");

    // Component IDs should be preserved
    let reconciled_ids: std::collections::HashSet<String> = db
        .all_components()
        .unwrap()
        .iter()
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(
        original_ids, reconciled_ids,
        "component IDs should survive reindex"
    );

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
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
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
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

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
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

    let result = sutra::tools::components::handle(&db, false).unwrap();
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
    let a1 = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/a2.rs", "rust", "h2", 50, true)
        .unwrap();
    let a3 = db
        .upsert_file("src/core/a3.rs", "rust", "h3", 50, true)
        .unwrap();
    let b1 = db
        .upsert_file("src/tools/b1.rs", "rust", "h4", 50, true)
        .unwrap();
    let b2 = db
        .upsert_file("src/tools/b2.rs", "rust", "h5", 50, true)
        .unwrap();
    let b3 = db
        .upsert_file("src/tools/b3.rs", "rust", "h6", 50, true)
        .unwrap();

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
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
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
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert_eq!(count, 2, "reconciliation should produce 2 components");

    let reconciled_comps = db.all_components().unwrap();
    let reconciled_ids: std::collections::HashSet<String> =
        reconciled_comps.iter().map(|c| c.id.clone()).collect();

    assert_eq!(
        original_ids, reconciled_ids,
        "component IDs should be preserved"
    );

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

fn pin_resolution_and_threshold(dir: &std::path::Path, threshold: f64) {
    let sutra_dir = dir.join(".sutra");
    std::fs::create_dir_all(&sutra_dir).unwrap();
    std::fs::write(
        sutra_dir.join("components.toml"),
        format!("resolution = 1.0\nstaleness_threshold = {threshold}"),
    )
    .unwrap();
}

#[test]
fn test_dissolved_components_hidden_from_queries() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert_eq!(db.all_components().unwrap().len(), 2);

    // Reindex and only re-insert one cluster (tools files removed)
    db.reindex().unwrap();
    let a1 = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/a2.rs", "rust", "h2", 50, true)
        .unwrap();
    let a3 = db
        .upsert_file("src/core/a3.rs", "rust", "h3", 50, true)
        .unwrap();
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
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

    // Only 1 active component visible
    let active = db.all_components().unwrap();
    assert_eq!(active.len(), 1, "dissolved component should be hidden");
    assert_eq!(active[0].name, "core");

    // MCP tool should also return only 1
    let result = sutra::tools::components::handle(&db, false).unwrap();
    assert_eq!(result["total"].as_u64().unwrap(), 1);
}

#[test]
fn test_merge_event_detected() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    let original = db.all_components().unwrap();
    assert_eq!(original.len(), 2);

    // Reindex and re-insert all files, but now with cross-cluster refs
    // so Louvain merges everything into 1 cluster
    db.reindex().unwrap();

    let a1 = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/a2.rs", "rust", "h2", 50, true)
        .unwrap();
    let a3 = db
        .upsert_file("src/core/a3.rs", "rust", "h3", 50, true)
        .unwrap();
    let b1 = db
        .upsert_file("src/tools/b1.rs", "rust", "h4", 50, true)
        .unwrap();
    let b2 = db
        .upsert_file("src/tools/b2.rs", "rust", "h5", 50, true)
        .unwrap();
    let b3 = db
        .upsert_file("src/tools/b3.rs", "rust", "h6", 50, true)
        .unwrap();

    let sa1 = insert_symbol(&db, a1, "core_a1");
    let sa2 = insert_symbol(&db, a2, "core_a2");
    let sa3 = insert_symbol(&db, a3, "core_a3");
    let sb1 = insert_symbol(&db, b1, "tools_b1");
    let sb2 = insert_symbol(&db, b2, "tools_b2");
    let sb3 = insert_symbol(&db, b3, "tools_b3");

    // Dense refs across ALL files (merged clique)
    for &(from, to_sym) in &[
        (a1, sa2),
        (a1, sa3),
        (a1, sb1),
        (a1, sb2),
        (a1, sb3),
        (a2, sa1),
        (a2, sa3),
        (a2, sb1),
        (a2, sb2),
        (a2, sb3),
        (a3, sa1),
        (a3, sa2),
        (a3, sb1),
        (a3, sb2),
        (a3, sb3),
        (b1, sa1),
        (b1, sa2),
        (b1, sa3),
        (b1, sb2),
        (b1, sb3),
        (b2, sa1),
        (b2, sa2),
        (b2, sa3),
        (b2, sb1),
        (b2, sb3),
        (b3, sa1),
        (b3, sa2),
        (b3, sa3),
        (b3, sb1),
        (b3, sb2),
    ] {
        insert_refs(&db, from, to_sym, 10);
    }

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

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
    let a1 = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/a2.rs", "rust", "h2", 50, true)
        .unwrap();
    let a3 = db
        .upsert_file("src/core/a3.rs", "rust", "h3", 50, true)
        .unwrap();
    let b1 = db
        .upsert_file("src/tools/b1.rs", "rust", "h4", 50, true)
        .unwrap();
    let b2 = db
        .upsert_file("src/tools/b2.rs", "rust", "h5", 50, true)
        .unwrap();
    let b3 = db
        .upsert_file("src/tools/b3.rs", "rust", "h6", 50, true)
        .unwrap();

    let sa1 = insert_symbol(&db, a1, "core_a1");
    let sa2 = insert_symbol(&db, a2, "core_a2");
    let sa3 = insert_symbol(&db, a3, "core_a3");
    let sb1 = insert_symbol(&db, b1, "tools_b1");
    let sb2 = insert_symbol(&db, b2, "tools_b2");
    let sb3 = insert_symbol(&db, b3, "tools_b3");

    // Dense refs across ALL files
    for &(from, to_sym) in &[
        (a1, sa2),
        (a1, sa3),
        (a1, sb1),
        (a1, sb2),
        (a1, sb3),
        (a2, sa1),
        (a2, sa3),
        (a2, sb1),
        (a2, sb2),
        (a2, sb3),
        (a3, sa1),
        (a3, sa2),
        (a3, sb1),
        (a3, sb2),
        (a3, sb3),
        (b1, sa1),
        (b1, sa2),
        (b1, sa3),
        (b1, sb2),
        (b1, sb3),
        (b2, sa1),
        (b2, sa2),
        (b2, sa3),
        (b2, sb1),
        (b2, sb3),
        (b3, sa1),
        (b3, sa2),
        (b3, sa3),
        (b3, sb1),
        (b3, sb2),
    ] {
        insert_refs(&db, from, to_sym, 10);
    }

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert_eq!(count, 1, "should be 1 big component");

    let original = db.all_components().unwrap();
    assert_eq!(original.len(), 1);
    let original_id = original[0].id.clone();

    // Reindex and split into 2 separate cliques (no cross-cluster refs)
    db.reindex().unwrap();
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

    // Original component should match one cluster (>60% overlap)
    // and split event should fire because its files span 2 clusters
    let active = db.all_components().unwrap();
    assert!(
        active.len() >= 2,
        "should have at least 2 components after split"
    );

    // Find events on the original component (if it survived)
    let events = db.component_events(&original_id).unwrap();
    let split_events: Vec<_> = events.iter().filter(|(t, _)| t == "split").collect();
    assert_eq!(split_events.len(), 1, "should have exactly 1 split event");

    let detail: serde_json::Value = serde_json::from_str(&split_events[0].1).unwrap();
    assert!(detail["clusters"].as_u64().unwrap() >= 2);

    // Split events must include target component IDs
    let targets = detail["targets"]
        .as_array()
        .expect("split detail should have targets array");
    assert!(
        targets.len() >= 2,
        "split should reference at least 2 target components"
    );
    for t in targets {
        assert!(
            t["component_id"].is_string(),
            "each target should have a component_id"
        );
        assert!(
            t["files"].is_number(),
            "each target should have a files count"
        );
    }
}

#[test]
fn test_drift_event_detected() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());

    // 2 clusters: A has 5 files, B has 5 files
    let a_files: Vec<i64> = (1..=5)
        .map(|i| {
            db.upsert_file(
                &format!("src/core/a{}.rs", i),
                "rust",
                &format!("ha{}", i),
                50,
                true,
            )
            .unwrap()
        })
        .collect();
    let b_files: Vec<i64> = (1..=5)
        .map(|i| {
            db.upsert_file(
                &format!("src/tools/b{}.rs", i),
                "rust",
                &format!("hb{}", i),
                50,
                true,
            )
            .unwrap()
        })
        .collect();

    // Symbols for each file
    let a_syms: Vec<i64> = a_files
        .iter()
        .enumerate()
        .map(|(i, &fid)| insert_symbol(&db, fid, &format!("core_a{}", i + 1)))
        .collect();
    let b_syms: Vec<i64> = b_files
        .iter()
        .enumerate()
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
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
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
                &format!("src/core/a{}.rs", i),
                "rust",
                &format!("ha{}", i),
                50,
                true,
            )
            .unwrap()
        })
        .collect();
    // B gets its 5 files + 2 from A
    let b_files2: Vec<i64> = (1..=5)
        .map(|i| {
            db.upsert_file(
                &format!("src/tools/b{}.rs", i),
                "rust",
                &format!("hb{}", i),
                50,
                true,
            )
            .unwrap()
        })
        .collect();
    let drift_files: Vec<i64> = (4..=5)
        .map(|i| {
            db.upsert_file(
                &format!("src/core/a{}.rs", i),
                "rust",
                &format!("ha{}", i),
                50,
                true,
            )
            .unwrap()
        })
        .collect();

    // Symbols
    let a_syms2: Vec<i64> = a_files2
        .iter()
        .enumerate()
        .map(|(i, &fid)| insert_symbol(&db, fid, &format!("core_a{}", i + 1)))
        .collect();
    let b_syms2: Vec<i64> = b_files2
        .iter()
        .enumerate()
        .map(|(i, &fid)| insert_symbol(&db, fid, &format!("tools_b{}", i + 1)))
        .collect();
    let drift_syms: Vec<i64> = drift_files
        .iter()
        .enumerate()
        .map(|(i, &fid)| insert_symbol(&db, fid, &format!("core_a{}", i + 4)))
        .collect();

    // Dense intra-cluster refs for A (3 files)
    for i in 0..3 {
        for j in 0..3 {
            if i != j {
                insert_refs(&db, a_files2[i], a_syms2[j], 10);
            }
        }
    }
    // Dense intra-cluster refs for B (5 files + 2 drifted files)
    let all_b = [b_files2.as_slice(), drift_files.as_slice()].concat();
    let all_b_syms = [b_syms2.as_slice(), drift_syms.as_slice()].concat();
    for i in 0..all_b.len() {
        for j in 0..all_b.len() {
            if i != j {
                insert_refs(&db, all_b[i], all_b_syms[j], 10);
            }
        }
    }

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

    // Component A should have a drift event (40% of its files moved to B)
    let events = db.component_events(&comp_a_id).unwrap();
    let drift_events: Vec<_> = events.iter().filter(|(t, _)| t == "drift").collect();
    assert_eq!(
        drift_events.len(),
        1,
        "should have exactly 1 drift event on comp A"
    );

    let detail: serde_json::Value = serde_json::from_str(&drift_events[0].1).unwrap();
    let ratio = detail["ratio"].as_f64().unwrap();
    assert!(ratio > 0.3, "drift ratio should be > 0.3, got {}", ratio);
    assert_eq!(detail["shifted_files"].as_u64().unwrap(), 2);

    // Regression: drift must NOT produce a spurious merge event on comp B
    let active = db.all_components().unwrap();
    for c in &active {
        let events = db.component_events(&c.id).unwrap();
        let merges: Vec<_> = events.iter().filter(|(t, _)| t == "merge").collect();
        assert!(
            merges.is_empty(),
            "drift scenario should not emit merge events, but {} has {:?}",
            c.name,
            merges
        );
    }
}

#[test]
fn test_unmatched_cluster_creates_new_component() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());

    // Start with 1 cluster (core: a1,a2,a3)
    let a1 = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/a2.rs", "rust", "h2", 50, true)
        .unwrap();
    let a3 = db
        .upsert_file("src/core/a3.rs", "rust", "h3", 50, true)
        .unwrap();
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
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    let original = db.all_components().unwrap();
    assert_eq!(original.len(), 1);
    let original_id = original[0].id.clone();

    // Reindex and add a second cluster (tools: b1,b2,b3) alongside the original
    db.reindex().unwrap();
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

    let active = db.all_components().unwrap();
    assert_eq!(
        active.len(),
        2,
        "original preserved + new component created"
    );

    // Original component should still exist
    assert!(
        active.iter().any(|c| c.id == original_id),
        "original component ID should be preserved"
    );

    // New component should have "tools" name
    let new_comp = active.iter().find(|c| c.id != original_id).unwrap();
    assert_eq!(new_comp.name, "tools");
}

// ---------------------------------------------------------------------------
// Staleness detection tests
// ---------------------------------------------------------------------------

#[test]
fn test_staleness_skips_when_graph_unchanged() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert_eq!(count, 2);

    // Metadata should be recorded
    let meta = db.clustering_meta().unwrap();
    assert!(meta.is_some(), "clustering metadata should be written");

    // Second call with identical graph should skip
    let count2 = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert_eq!(count2, 0, "should skip when graph is unchanged");
}

#[test]
fn test_staleness_detects_file_addition() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    let original_meta = db.clustering_meta().unwrap().unwrap();

    // Add a new file with refs into cluster A
    let new_file = db
        .upsert_file("src/core/a4.rs", "rust", "h7", 50, true)
        .unwrap();
    let new_sym = insert_symbol(&db, new_file, "core_a4");
    let a1 = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    insert_refs(&db, new_file, insert_symbol(&db, a1, "core_a1_v2"), 10);
    insert_refs(&db, a1, new_sym, 10);

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert!(count > 0, "should recluster when files added");

    let new_meta = db.clustering_meta().unwrap().unwrap();
    assert_ne!(
        original_meta.1, new_meta.1,
        "file count should have changed in metadata"
    );
}

#[test]
fn test_staleness_detects_edge_change_above_threshold() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    let (stored_edges, _stored_files, _, _, _) = db.clustering_meta().unwrap().unwrap();

    // Add cross-cluster edges exceeding 10% of stored_edges
    assert!(
        stored_edges > 0,
        "precondition: should have edges after clustering"
    );
    let a1_id = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    let b1_id = db
        .upsert_file("src/tools/b1.rs", "rust", "h4", 50, true)
        .unwrap();
    let b2_id = db
        .upsert_file("src/tools/b2.rs", "rust", "h5", 50, true)
        .unwrap();
    let sa1 = insert_symbol(&db, a1_id, "core_a1_extra");
    insert_refs(&db, b1_id, sa1, 5);
    insert_refs(&db, b2_id, sa1, 5);

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert!(
        count > 0,
        "should recluster when edge count changed by >10%"
    );

    let (new_edges, _, _, _, _) = db.clustering_meta().unwrap().unwrap();
    assert!(
        new_edges > stored_edges,
        "new edge count ({new_edges}) should exceed stored ({stored_edges})"
    );
}

#[test]
fn test_staleness_ignores_edge_change_below_threshold() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());

    // Build a large graph so a single new edge is <10%
    // 10 files in cluster A, 10 files in cluster B -> many edges
    let mut a_files = Vec::new();
    let mut a_syms = Vec::new();
    for i in 1..=10 {
        let fid = db
            .upsert_file(
                &format!("src/core/a{i}.rs"),
                "rust",
                &format!("ha{i}"),
                50,
                true,
            )
            .unwrap();
        let sid = insert_symbol(&db, fid, &format!("core_a{i}"));
        a_files.push(fid);
        a_syms.push(sid);
    }
    let mut b_files = Vec::new();
    let mut b_syms = Vec::new();
    for i in 1..=10 {
        let fid = db
            .upsert_file(
                &format!("src/tools/b{i}.rs"),
                "rust",
                &format!("hb{i}"),
                50,
                true,
            )
            .unwrap();
        let sid = insert_symbol(&db, fid, &format!("tools_b{i}"));
        b_files.push(fid);
        b_syms.push(sid);
    }

    // Dense intra-cluster refs (every pair)
    for i in 0..10 {
        for j in 0..10 {
            if i != j {
                insert_refs(&db, a_files[i], a_syms[j], 5);
                insert_refs(&db, b_files[i], b_syms[j], 5);
            }
        }
    }

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    let (stored_edges, _, _, _, _) = db.clustering_meta().unwrap().unwrap();
    // 10 files * 9 neighbors / 2 = 45 undirected edges per cluster, 90 total
    assert!(
        stored_edges >= 80,
        "should have many edges, got {stored_edges}"
    );

    // Add 1 cross-cluster edge (~1% change)
    insert_refs(&db, a_files[0], b_syms[0], 1);

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert_eq!(count, 0, "should skip when edge change is below threshold");
}

#[test]
fn test_staleness_threshold_override() {
    let (dir, db) = setup_db();
    // Set a very high threshold so even significant changes are ignored
    pin_resolution_and_threshold(dir.path(), 0.5);
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

    // Add cross-cluster refs that would exceed 10% but not 50%
    let a1_id = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    let b1_id = db
        .upsert_file("src/tools/b1.rs", "rust", "h4", 50, true)
        .unwrap();
    let sa1 = insert_symbol(&db, a1_id, "core_a1_extra");
    insert_refs(&db, b1_id, sa1, 5);

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert_eq!(
        count, 0,
        "should skip when change is below custom 50% threshold"
    );
}

#[test]
fn test_clustering_meta_survives_reindex() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    let meta_before = db.clustering_meta().unwrap();
    assert!(meta_before.is_some());

    db.reindex().unwrap();

    let meta_after = db.clustering_meta().unwrap();
    assert_eq!(
        meta_before, meta_after,
        "clustering metadata should survive reindex"
    );
}

#[test]
fn test_first_run_records_metadata() {
    let (dir, db) = setup_db();

    let a1 = db.upsert_file("src/a.rs", "rust", "h1", 50, true).unwrap();
    let a2 = db.upsert_file("src/b.rs", "rust", "h2", 50, true).unwrap();
    let sa1 = insert_symbol(&db, a1, "fn_a");
    insert_refs(&db, a2, sa1, 5);

    // No metadata before first run
    assert!(db.clustering_meta().unwrap().is_none());

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

    let meta = db.clustering_meta().unwrap();
    assert!(
        meta.is_some(),
        "metadata should be recorded after first clustering"
    );
    let (edge_count, file_count, _, _, _) = meta.unwrap();
    assert_eq!(file_count, 2);
    assert!(edge_count > 0);
}

// ---------------------------------------------------------------------------
// Semantic anchor tests
// ---------------------------------------------------------------------------

#[test]
fn test_semantic_anchors_computed_after_discovery() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

    let anchor_count = components::compute_semantic_anchors(&db, dir.path()).unwrap();
    assert!(
        anchor_count > 0,
        "should compute anchors for discovered components"
    );

    let comps = db.all_components().unwrap();
    for c in &comps {
        let anchors = db.anchors_for_component(&c.id).unwrap();
        assert!(
            !anchors.is_empty(),
            "component '{}' should have at least one anchor",
            c.name
        );
        assert!(
            anchors.len() >= 3 && anchors.len() <= 7,
            "component '{}' has {} anchors, expected 3-7",
            c.name,
            anchors.len()
        );
        for a in &anchors {
            assert!(a.score.is_some());
            assert!(a.rationale.is_some());
        }
    }
}

#[test]
fn test_anchors_prefer_high_in_degree() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());

    // Single component: 5 files under src/lib/
    let f1 = db
        .upsert_file("src/lib/a.rs", "rust", "h1", 50, true)
        .unwrap();
    let f2 = db
        .upsert_file("src/lib/b.rs", "rust", "h2", 50, true)
        .unwrap();
    let f3 = db
        .upsert_file("src/lib/c.rs", "rust", "h3", 50, true)
        .unwrap();
    let f4 = db
        .upsert_file("src/lib/d.rs", "rust", "h4", 50, true)
        .unwrap();
    let f5 = db
        .upsert_file("src/lib/e.rs", "rust", "h5", 50, true)
        .unwrap();

    // "popular" symbol: called from all other files
    let popular = insert_symbol(&db, f1, "popular_fn");
    let _s2 = insert_symbol(&db, f2, "helper_b");
    let _s3 = insert_symbol(&db, f3, "helper_c");
    let _s4 = insert_symbol(&db, f4, "helper_d");
    let _s5 = insert_symbol(&db, f5, "helper_e");

    // Dense cross-refs to form one component
    insert_refs(&db, f2, popular, 20);
    insert_refs(&db, f3, popular, 20);
    insert_refs(&db, f4, popular, 20);
    insert_refs(&db, f5, popular, 20);
    // Some intra-cluster refs to glue the cluster
    let s2 = insert_symbol(&db, f2, "bridge_b");
    insert_refs(&db, f1, s2, 5);
    insert_refs(&db, f3, s2, 5);
    insert_refs(&db, f4, s2, 5);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

    let comps = db.all_components().unwrap();
    assert!(!comps.is_empty());

    components::compute_semantic_anchors(&db, dir.path()).unwrap();

    // Find the component containing f1
    let anchors = db.anchors_for_component(&comps[0].id).unwrap();
    assert!(!anchors.is_empty());
    // The top-ranked anchor should be "popular_fn" due to high in-degree
    assert_eq!(
        anchors[0].symbol_name, "popular_fn",
        "highest in-degree symbol should rank first"
    );
}

#[test]
fn test_anchor_count_adaptive() {
    // Unit test for the anchor_count function
    assert_eq!(components::anchor_count(5), 3, "small: min 3");
    assert_eq!(
        components::anchor_count(10),
        3,
        "10 eligible: 10/8=1 → clamp to 3"
    );
    assert_eq!(components::anchor_count(24), 3, "24 eligible: 24/8=3");
    assert_eq!(components::anchor_count(40), 5, "40 eligible: 40/8=5");
    assert_eq!(components::anchor_count(56), 7, "56 eligible: 56/8=7");
    assert_eq!(components::anchor_count(100), 7, "large: max 7");
}

#[test]
fn test_anchors_exclude_non_anchor_kinds() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());

    let f1 = db
        .upsert_file("src/pkg/a.rs", "rust", "h1", 50, true)
        .unwrap();
    let f2 = db
        .upsert_file("src/pkg/b.rs", "rust", "h2", 50, true)
        .unwrap();

    // Anchor-eligible: function, struct
    let fn_sym = insert_symbol(&db, f1, "my_function");
    let _struct_sym = insert_symbol_with_kind(&db, f1, "MyStruct", "MyStruct", "struct");
    // Non-eligible: module, import
    let _mod_sym = insert_symbol_with_kind(&db, f2, "my_module", "my_module", "module");

    // Cross-refs to form cluster
    insert_refs(&db, f2, fn_sym, 10);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    components::compute_semantic_anchors(&db, dir.path()).unwrap();

    let comps = db.all_components().unwrap();
    let all_anchors = db.all_anchors_grouped().unwrap();
    for c in &comps {
        if let Some(anchors) = all_anchors.get(&c.id) {
            let names: HashSet<&str> = anchors.iter().map(|a| a.symbol_name.as_str()).collect();
            assert!(
                !names.contains("my_module"),
                "module symbols should not be anchors"
            );
        }
    }
}

#[test]
fn test_anchors_in_components_tool_output() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    components::compute_semantic_anchors(&db, dir.path()).unwrap();

    let result = sutra::tools::components::handle(&db, false).unwrap();
    let comps = result["components"].as_array().unwrap();
    for c in comps {
        let anchors = c["anchors"].as_array().unwrap();
        assert!(!anchors.is_empty(), "MCP output should include anchors");
        for a in anchors {
            assert!(a["symbol"].is_string());
            assert!(a["score"].is_number());
            assert!(a["rationale"].is_string());
        }
    }
}

#[test]
fn test_anchors_recomputed_on_recluster() {
    let (dir, db) = setup_db();
    pin_resolution_and_threshold(dir.path(), 0.05);
    setup_two_clusters(&db);

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    components::compute_semantic_anchors(&db, dir.path()).unwrap();

    let _comps = db.all_components().unwrap();
    let original_anchors: HashSet<String> = {
        let grouped = db.all_anchors_grouped().unwrap();
        grouped
            .values()
            .flatten()
            .map(|a| a.symbol_name.clone())
            .collect()
    };
    assert!(!original_anchors.is_empty());

    // Reindex wipes ephemeral tables
    db.reindex().unwrap();

    // Re-insert the same clusters plus a new file with a new popular symbol
    setup_two_clusters(&db);
    let new_file = db
        .upsert_file("src/core/new.rs", "rust", "hnew", 50, true)
        .unwrap();
    let new_sym = insert_symbol(&db, new_file, "new_popular_fn");
    // Make new_sym heavily referenced within core cluster
    let a1 = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    insert_refs(&db, a1, new_sym, 50);

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();
    assert!(count > 0, "should recluster after adding new file");

    components::compute_semantic_anchors(&db, dir.path()).unwrap();

    let new_anchors: HashSet<String> = {
        let grouped = db.all_anchors_grouped().unwrap();
        grouped
            .values()
            .flatten()
            .map(|a| a.symbol_name.clone())
            .collect()
    };
    assert!(
        new_anchors.contains("new_popular_fn"),
        "recomputed anchors should include the new popular symbol"
    );
}

// ---------------------------------------------------------------------------
// Concept density tests
// ---------------------------------------------------------------------------

#[test]
fn test_concept_density_in_tool_output() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());

    // Cluster A (diverse): function + struct + enum, varied names, 30 LOC per file
    let a1 = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 30, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/a2.rs", "rust", "h2", 30, true)
        .unwrap();
    let a3 = db
        .upsert_file("src/core/a3.rs", "rust", "h3", 30, true)
        .unwrap();

    let sa1 = insert_symbol_with_kind(&db, a1, "UserProfile", "UserProfile", "struct");
    let sa2 = insert_symbol_with_kind(&db, a2, "fetch_data", "fetch_data", "function");
    let sa3 = insert_symbol_with_kind(&db, a3, "RenderMode", "RenderMode", "enum");

    // Cluster B (repetitive): all functions, similar names, 30 LOC per file
    let b1 = db
        .upsert_file("src/handlers/b1.rs", "rust", "h4", 30, true)
        .unwrap();
    let b2 = db
        .upsert_file("src/handlers/b2.rs", "rust", "h5", 30, true)
        .unwrap();
    let b3 = db
        .upsert_file("src/handlers/b3.rs", "rust", "h6", 30, true)
        .unwrap();

    let sb1 = insert_symbol_with_kind(&db, b1, "handle_create", "handle_create", "function");
    let sb2 = insert_symbol_with_kind(&db, b2, "handle_update", "handle_update", "function");
    let sb3 = insert_symbol_with_kind(&db, b3, "handle_delete", "handle_delete", "function");

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

    let files = db.all_files().unwrap();
    components::discover_components(&db, &files, dir.path(), &HashMap::new()).unwrap();

    let result = sutra::tools::components::handle(&db, false).unwrap();
    let comps = result["components"].as_array().unwrap();
    assert_eq!(comps.len(), 2);

    let mut densities: Vec<(String, f64)> = comps
        .iter()
        .map(|c| {
            let name = c["name"].as_str().unwrap().to_string();
            let density = c["concept_density"]
                .as_f64()
                .expect("concept_density should be present");
            assert!(density >= 0.0, "density should be non-negative");
            (name, density)
        })
        .collect();
    densities.sort_by_key(|(n, _)| n.clone());

    let diverse = densities.iter().find(|(n, _)| n == "core").unwrap();
    let repetitive = densities.iter().find(|(n, _)| n == "handlers").unwrap();

    assert!(
        diverse.1 > repetitive.1,
        "diverse component ({}: {}) should have higher density than repetitive ({}: {})",
        diverse.0,
        diverse.1,
        repetitive.0,
        repetitive.1,
    );
}

#[test]
fn test_boundary_hints_boost_co_module_edges() {
    let (dir, db) = setup_db();

    // Two files in the same directory (co-module)
    let a1 = db
        .upsert_file("src/core/a1.rs", "rust", "h1", 50, true)
        .unwrap();
    let a2 = db
        .upsert_file("src/core/a2.rs", "rust", "h2", 50, true)
        .unwrap();

    // Two files in different directories
    let b1 = db
        .upsert_file("src/tools/b1.rs", "rust", "h3", 50, true)
        .unwrap();
    let b2 = db
        .upsert_file("src/other/b2.rs", "rust", "h4", 50, true)
        .unwrap();

    // Extra files to allow clustering
    let a3 = db
        .upsert_file("src/core/a3.rs", "rust", "h5", 50, true)
        .unwrap();
    let b3 = db
        .upsert_file("src/tools/b3.rs", "rust", "h6", 50, true)
        .unwrap();

    let sa1 = insert_symbol(&db, a1, "core_fn1");
    let sa2 = insert_symbol(&db, a2, "core_fn2");
    let sa3 = insert_symbol(&db, a3, "core_fn3");
    let sb1 = insert_symbol(&db, b1, "tools_fn1");
    let sb2 = insert_symbol(&db, b2, "other_fn2");
    let sb3 = insert_symbol(&db, b3, "tools_fn3");

    // Moderate cross-refs between co-module files
    insert_refs(&db, a1, sa2, 3);
    insert_refs(&db, a2, sa1, 3);
    insert_refs(&db, a1, sa3, 3);
    insert_refs(&db, a3, sa1, 3);
    insert_refs(&db, a2, sa3, 3);
    insert_refs(&db, a3, sa2, 3);

    // Same moderate cross-refs between non-co-module files
    insert_refs(&db, b1, sb2, 3);
    insert_refs(&db, b2, sb1, 3);
    insert_refs(&db, b1, sb3, 3);
    insert_refs(&db, b3, sb1, 3);
    insert_refs(&db, b2, sb3, 3);
    insert_refs(&db, b3, sb2, 3);

    let mut multipliers = HashMap::new();
    multipliers.insert("rust".to_string(), 2.0);

    let files = db.all_files().unwrap();
    let count = components::discover_components(&db, &files, dir.path(), &multipliers).unwrap();
    assert!(count >= 2, "should discover components with boundary hints");
}
