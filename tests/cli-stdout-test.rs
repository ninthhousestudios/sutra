//! CLI stdout carries only command output (sutra/490).
//!
//! `sutra check` refreshes a stale index before evaluating, and the refresh
//! logs at INFO. Those logs must go to stderr, or `--format json` output is
//! unparseable exactly when the index was stale — the common CI case.

use std::path::Path;
use std::process::{Command, Output};

use sutra::workspace::{WorkspaceEntry, WorkspacesConfig, save_workspaces};

fn git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn sutra(cwd: &Path, db_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sutra"))
        .current_dir(cwd)
        .env_remove("RUST_LOG")
        .env("SUTRA_LOG_LEVEL", "info")
        .env("SUTRA_DB_DIR", db_dir)
        .env("SUTRA_WORKSPACES", db_dir.join("workspaces.toml"))
        .args(args)
        .output()
        .expect("sutra spawn")
}

#[test]
fn check_json_stdout_parses_after_logging_refresh() {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    std::fs::write(
        r.join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(r.join("src")).unwrap();
    std::fs::write(r.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
    git(r, &["init", "-q"]);
    git(r, &["config", "user.email", "test@example.com"]);
    git(r, &["config", "user.name", "Test"]);
    git(r, &["add", "-A"]);
    git(r, &["commit", "-q", "--no-verify", "-m", "seed"]);
    // `--diff HEAD` diffs against HEAD's parent, so HEAD needs one.
    std::fs::write(r.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();
    git(r, &["commit", "-qam", "second", "--no-verify"]);

    let db_dir = tempfile::tempdir().unwrap();
    let ws_config = WorkspacesConfig {
        workspace: vec![WorkspaceEntry {
            id: "stdout".to_string(),
            root: r.to_path_buf(),
            languages: vec!["rust".to_string()],
            frozen: false,
        }],
    };
    save_workspaces(&db_dir.path().join("workspaces.toml"), &ws_config).unwrap();

    let parse = sutra(r, db_dir.path(), &["parse", "stdout"]);
    assert!(
        parse.status.success(),
        "parse failed: {}",
        String::from_utf8_lossy(&parse.stderr)
    );

    // Drift the working tree so check takes the logging incremental refresh.
    std::fs::write(
        r.join("src/lib.rs"),
        "pub fn a() {}\npub fn b() {}\npub fn c() {}\n",
    )
    .unwrap();

    let check = sutra(
        r,
        db_dir.path(),
        &["check", "--diff", "HEAD", "--format", "json"],
    );
    let stdout = String::from_utf8_lossy(&check.stdout);
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(
        stderr.contains("INFO"),
        "fixture must exercise a refresh that logs; stderr was: {stderr}"
    );
    serde_json::from_str::<serde_json::Value>(&stdout).unwrap_or_else(|e| {
        panic!("check stdout is not JSON ({e}):\n{stdout}\n--- stderr ---\n{stderr}")
    });
}
