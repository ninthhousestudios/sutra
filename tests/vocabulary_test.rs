use sutra::components;
use sutra::db::{Db, InsertSymbolParams};
use sutra::vocabulary;

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

fn insert_symbol(db: &Db, file_id: i64, qualified: &str, short: &str, kind: &str) -> i64 {
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

fn pin_resolution(dir: &std::path::Path) {
    let sutra_dir = dir.join(".sutra");
    std::fs::create_dir_all(&sutra_dir).unwrap();
    std::fs::write(sutra_dir.join("components.toml"), "resolution = 1.0").unwrap();
}

fn setup_component(db: &Db, dir: &std::path::Path) {
    let a1 = db.upsert_file("src/auth/a1.rs", "rust", "h1", 50, true).unwrap();
    let a2 = db.upsert_file("src/auth/a2.rs", "rust", "h2", 50, true).unwrap();
    let a3 = db.upsert_file("src/auth/a3.rs", "rust", "h3", 50, true).unwrap();

    let sa1 = insert_symbol(db, a1, "auth_login", "auth_login", "function");
    let sa2 = insert_symbol(db, a2, "auth_verify", "auth_verify", "function");
    let sa3 = insert_symbol(db, a3, "auth_logout", "auth_logout", "function");

    insert_refs(db, a1, sa2, 10);
    insert_refs(db, a1, sa3, 10);
    insert_refs(db, a2, sa1, 10);
    insert_refs(db, a2, sa3, 10);
    insert_refs(db, a3, sa1, 10);
    insert_refs(db, a3, sa2, 10);

    let files = db.all_files().unwrap();
    components::discover_components(db, &files, dir).unwrap();
    components::compute_semantic_anchors(db, dir).unwrap();
}

// ---------------------------------------------------------------------------
// TOML parsing
// ---------------------------------------------------------------------------

#[test]
fn test_parse_aliases_produces_correct_entries() {
    let toml = r#"
[component]
auth = "authentication"

[file]
config = "src/config.rs"

[symbol]
UP = "UserProfile"
"#;
    let config = vocabulary::parse_aliases(toml).unwrap();
    assert_eq!(config.component["auth"], "authentication");
    assert_eq!(config.file["config"], "src/config.rs");
    assert_eq!(config.symbol["UP"], "UserProfile");
}

// ---------------------------------------------------------------------------
// Sync round-trip
// ---------------------------------------------------------------------------

#[test]
fn test_sync_aliases_from_toml() {
    let (dir, db) = setup_db();
    let sutra_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&sutra_dir).unwrap();
    std::fs::write(
        sutra_dir.join("aliases.toml"),
        r#"
[component]
auth = "authentication"

[file]
config = "src/config.rs"
"#,
    )
    .unwrap();

    let count = vocabulary::sync_aliases(&db, dir.path()).unwrap();
    assert_eq!(count, 2);

    let aliases = db.all_aliases().unwrap();
    assert_eq!(aliases.len(), 2);

    let auth = aliases.iter().find(|a| a.term == "auth").unwrap();
    assert_eq!(auth.target_kind, "component");
    assert_eq!(auth.target_ref, "authentication");

    let config = aliases.iter().find(|a| a.term == "config").unwrap();
    assert_eq!(config.target_kind, "file");
    assert_eq!(config.target_ref, "src/config.rs");
}

#[test]
fn test_sync_aliases_survives_reindex() {
    let (dir, db) = setup_db();
    let sutra_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&sutra_dir).unwrap();
    std::fs::write(
        sutra_dir.join("aliases.toml"),
        "[component]\nauth = \"authentication\"\n",
    )
    .unwrap();

    vocabulary::sync_aliases(&db, dir.path()).unwrap();
    assert_eq!(db.all_aliases().unwrap().len(), 1);

    db.reindex().unwrap();

    // Aliases are durable — should survive reindex
    assert_eq!(db.all_aliases().unwrap().len(), 1);
}

#[test]
fn test_sync_replaces_on_update() {
    let (dir, db) = setup_db();
    let sutra_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&sutra_dir).unwrap();

    std::fs::write(
        sutra_dir.join("aliases.toml"),
        "[component]\nauth = \"old\"\n",
    )
    .unwrap();
    vocabulary::sync_aliases(&db, dir.path()).unwrap();
    assert_eq!(db.find_alias("auth").unwrap().unwrap().target_ref, "old");

    std::fs::write(
        sutra_dir.join("aliases.toml"),
        "[component]\nauth = \"new\"\n",
    )
    .unwrap();
    vocabulary::sync_aliases(&db, dir.path()).unwrap();
    assert_eq!(db.find_alias("auth").unwrap().unwrap().target_ref, "new");
    assert_eq!(db.all_aliases().unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// Resolution priority
// ---------------------------------------------------------------------------

#[test]
fn test_resolve_alias_wins_over_component_name() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_component(&db, dir.path());

    // Component is named "auth" — set up an alias "auth" pointing to a file instead
    let sutra_dir = dir.path().join(".sutra");
    std::fs::write(
        sutra_dir.join("aliases.toml"),
        "[file]\nauth = \"src/auth/a1.rs\"\n",
    )
    .unwrap();
    vocabulary::sync_aliases(&db, dir.path()).unwrap();

    let matches = vocabulary::resolve(&db, "auth").unwrap();
    assert!(!matches.is_empty(), "should find matches for 'auth'");
    assert_eq!(matches[0].source, "alias", "alias should be first match");
    assert_eq!(matches[0].target_kind, "file");

    // Component name match should come second
    let comp_match = matches.iter().find(|m| m.source == "component");
    assert!(comp_match.is_some(), "component name should also match");
}

#[test]
fn test_resolve_component_wins_over_anchor() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_component(&db, dir.path());

    // "auth" matches the component name AND anchor names (auth_login, auth_verify, etc.)
    let matches = vocabulary::resolve(&db, "auth").unwrap();

    let sources: Vec<&str> = matches.iter().map(|m| m.source.as_str()).collect();
    let first_comp = sources.iter().position(|s| *s == "component");
    let first_anchor = sources.iter().position(|s| *s == "anchor");

    assert!(first_comp.is_some(), "should have component match");
    assert!(first_anchor.is_some(), "should have anchor matches");
    assert!(
        first_comp.unwrap() < first_anchor.unwrap(),
        "component match should come before anchor matches"
    );
}

// ---------------------------------------------------------------------------
// Orphan detection
// ---------------------------------------------------------------------------

#[test]
fn test_orphan_detected_for_dissolved_component() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_component(&db, dir.path());

    let comps = db.all_components().unwrap();
    let comp = &comps[0];

    // Alias targeting this component by name
    let sutra_dir = dir.path().join(".sutra");
    std::fs::write(
        sutra_dir.join("aliases.toml"),
        &format!("[component]\nmy-auth = \"{}\"\n", comp.name),
    )
    .unwrap();
    vocabulary::sync_aliases(&db, dir.path()).unwrap();

    // Resolve — should NOT be orphan
    let matches = vocabulary::resolve(&db, "my-auth").unwrap();
    assert!(!matches.is_empty());
    assert!(!matches[0].orphan, "should not be orphan before dissolve");

    // Dissolve the component
    db.dissolve_component(&comp.id).unwrap();

    // Resolve — should be orphan now
    let matches = vocabulary::resolve(&db, "my-auth").unwrap();
    assert!(!matches.is_empty());
    assert!(matches[0].orphan, "should be orphan after dissolve");
}

#[test]
fn test_orphan_detected_for_missing_file() {
    let (dir, db) = setup_db();

    let sutra_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&sutra_dir).unwrap();
    std::fs::write(
        sutra_dir.join("aliases.toml"),
        "[file]\nmissing = \"src/does_not_exist.rs\"\n",
    )
    .unwrap();
    vocabulary::sync_aliases(&db, dir.path()).unwrap();

    let matches = vocabulary::resolve(&db, "missing").unwrap();
    assert_eq!(matches.len(), 1);
    assert!(matches[0].orphan, "file alias to non-indexed path should be orphan");
}

// ---------------------------------------------------------------------------
// MCP tool output
// ---------------------------------------------------------------------------

#[test]
fn test_resolve_tool_output_shape() {
    let (dir, db) = setup_db();
    pin_resolution(dir.path());
    setup_component(&db, dir.path());

    let sutra_dir = dir.path().join(".sutra");
    std::fs::write(
        sutra_dir.join("aliases.toml"),
        "[file]\nmissing = \"src/gone.rs\"\n",
    )
    .unwrap();
    vocabulary::sync_aliases(&db, dir.path()).unwrap();

    // Resolve a term that hits alias (orphan) + component + anchors
    let result = vocabulary::resolve_to_json(&db, "auth").unwrap();
    assert!(result["query"].as_str().unwrap() == "auth");
    assert!(result["matches"].is_array());
    assert!(result["orphans"].is_array());

    // Resolve the orphan alias
    let result = vocabulary::resolve_to_json(&db, "missing").unwrap();
    assert_eq!(result["matches"].as_array().unwrap().len(), 0);
    assert_eq!(result["orphans"].as_array().unwrap().len(), 1);
}
