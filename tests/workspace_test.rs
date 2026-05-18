use std::path::PathBuf;

use sutra::workspace::{self, WorkspaceEntry};

fn entry(id: &str, root: &str, langs: &[&str]) -> WorkspaceEntry {
    WorkspaceEntry {
        id: id.to_string(),
        root: PathBuf::from(root),
        languages: langs.iter().map(|s| s.to_string()).collect(),
    }
}

/// Write a raw TOML string to the temp file path and load it back.
#[test]
fn test_parse_valid_toml() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");

    let toml = r#"
[[workspace]]
id = "alpha"
root = "/code/alpha"
languages = ["rust", "toml"]

[[workspace]]
id = "beta"
root = "/code/beta"
languages = ["dart"]
"#;
    std::fs::write(&path, toml).unwrap();

    let config = workspace::load_workspaces(&path).unwrap();
    assert_eq!(config.workspace.len(), 2);

    let alpha = &config.workspace[0];
    assert_eq!(alpha.id, "alpha");
    assert_eq!(alpha.root, PathBuf::from("/code/alpha"));
    assert_eq!(alpha.languages, vec!["rust", "toml"]);

    let beta = &config.workspace[1];
    assert_eq!(beta.id, "beta");
    assert_eq!(beta.languages, vec!["dart"]);
}

/// TOML with a missing required field (`languages`) should produce a parse
/// error.
#[test]
fn test_parse_missing_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");

    // `languages` is required — omitting it should cause a deserialisation error.
    let toml = r#"
[[workspace]]
id = "broken"
root = "/code/broken"
"#;
    std::fs::write(&path, toml).unwrap();

    let result = workspace::load_workspaces(&path);
    assert!(
        result.is_err(),
        "expected error for missing `languages` field"
    );
}

/// Adding the same workspace id twice must return an error on the second call.
#[test]
fn test_duplicate_workspace_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");

    workspace::add_workspace(&path, entry("dup", "/code/dup", &["rust"])).unwrap();
    let result = workspace::add_workspace(&path, entry("dup", "/code/other", &["dart"]));
    assert!(result.is_err(), "expected error for duplicate id");
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("dup"), "error message should mention the id");
}

/// Add a workspace, verify it is present, remove it, verify it is absent.
#[test]
fn test_add_remove_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");

    workspace::add_workspace(&path, entry("myws", "/code/myws", &["rust"])).unwrap();

    let entries = workspace::list_workspaces(&path).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "myws");

    workspace::remove_workspace(&path, "myws").unwrap();

    let entries = workspace::list_workspaces(&path).unwrap();
    assert!(entries.is_empty(), "workspace should be gone after removal");
}

/// Adding a workspace whose root contains an existing workspace's root must
/// fail — overlapping roots cause concurrent reparses to race
/// against the same files (see docs/reviews/2026-05-08-scheduler-wedge-bug.md).
#[test]
fn test_reject_ancestor_root_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");

    workspace::add_workspace(&path, entry("inner", "/home/u/proj", &["rust"])).unwrap();
    let result = workspace::add_workspace(&path, entry("outer", "/home/u", &["rust"]));
    assert!(
        result.is_err(),
        "expected error registering an ancestor of an existing workspace"
    );
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("overlap"), "error must mention overlap: {msg}");
}

/// And the reverse: registering a descendant of an existing workspace.
#[test]
fn test_reject_descendant_root_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");

    workspace::add_workspace(&path, entry("outer", "/home/u", &["rust"])).unwrap();
    let result = workspace::add_workspace(&path, entry("inner", "/home/u/proj", &["rust"]));
    assert!(
        result.is_err(),
        "expected error registering a descendant of an existing workspace"
    );
}

/// Identical roots also overlap.
#[test]
fn test_reject_identical_root() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");

    workspace::add_workspace(&path, entry("a", "/code/proj", &["rust"])).unwrap();
    let result = workspace::add_workspace(&path, entry("b", "/code/proj", &["rust"]));
    assert!(result.is_err(), "expected error for identical root");
}

/// Symlinked paths should be detected as overlapping after canonicalization.
#[test]
fn test_reject_symlink_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");
    let real_dir = dir.path().join("real_project");
    std::fs::create_dir_all(&real_dir).unwrap();
    let link_dir = dir.path().join("link_project");
    std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

    workspace::add_workspace(&path, entry("real", real_dir.to_str().unwrap(), &["rust"])).unwrap();
    let result =
        workspace::add_workspace(&path, entry("link", link_dir.to_str().unwrap(), &["rust"]));
    assert!(
        result.is_err(),
        "symlinked paths should be detected as overlapping"
    );
}

/// Non-canonical /../ paths should be detected as overlapping.
#[test]
fn test_reject_dotdot_overlap() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");
    let proj = dir.path().join("projects").join("alpha");
    std::fs::create_dir_all(&proj).unwrap();
    let sibling = dir.path().join("projects").join("beta");
    std::fs::create_dir_all(&sibling).unwrap();
    let dotdot = sibling.join("..").join("alpha");

    workspace::add_workspace(&path, entry("a", proj.to_str().unwrap(), &["rust"])).unwrap();
    let result = workspace::add_workspace(&path, entry("b", dotdot.to_str().unwrap(), &["rust"]));
    assert!(
        result.is_err(),
        "/../ path should be detected as overlapping"
    );
}

#[test]
fn test_resolve_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");
    workspace::add_workspace(&path, entry("proj", "/home/u/proj", &["rust"])).unwrap();
    let config = workspace::load_workspaces(&path).unwrap();
    assert_eq!(
        workspace::resolve_workspace(&config, "proj").unwrap().id,
        "proj"
    );
}

#[test]
fn test_resolve_by_root_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");
    workspace::add_workspace(&path, entry("proj", "/home/u/proj", &["rust"])).unwrap();
    let config = workspace::load_workspaces(&path).unwrap();
    assert_eq!(
        workspace::resolve_workspace(&config, "/home/u/proj")
            .unwrap()
            .id,
        "proj"
    );
}

#[test]
fn test_resolve_by_root_path_trailing_slash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");
    workspace::add_workspace(&path, entry("proj", "/home/u/proj", &["rust"])).unwrap();
    let config = workspace::load_workspaces(&path).unwrap();
    assert_eq!(
        workspace::resolve_workspace(&config, "/home/u/proj/")
            .unwrap()
            .id,
        "proj"
    );
}

#[test]
fn test_resolve_by_basename_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");
    workspace::add_workspace(&path, entry("proj", "/home/u/proj", &["rust"])).unwrap();
    let config = workspace::load_workspaces(&path).unwrap();
    assert_eq!(
        workspace::resolve_workspace(&config, "/some/other/path/proj")
            .unwrap()
            .id,
        "proj"
    );
}

#[test]
fn test_resolve_unknown_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");
    workspace::add_workspace(&path, entry("proj", "/home/u/proj", &["rust"])).unwrap();
    let config = workspace::load_workspaces(&path).unwrap();
    assert!(workspace::resolve_workspace(&config, "nonexistent").is_err());
}

/// Adding a workspace whose root path does not exist on disk should succeed —
/// sutra validates semantics, not filesystem presence at registration time.
#[test]
fn test_nonexistent_root() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workspaces.toml");

    let phantom = "/nonexistent/path/that/should/not/exist";
    workspace::add_workspace(&path, entry("ghost", phantom, &["rust"])).unwrap();

    let entries = workspace::list_workspaces(&path).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].root, PathBuf::from(phantom));
}
