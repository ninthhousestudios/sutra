//! A git repo written commit by commit and parsed into a fresh index, for the
//! review-time advisories' end-to-end tests. `mod support;` from a test file.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use sutra::config::Config;
use sutra::db::Db;
use sutra::parser::adapter::default_registry;
use sutra::pipeline;
use sutra::workspace::WorkspaceEntry;

pub struct Fixture {
    pub root: tempfile::TempDir,
    _db_dir: tempfile::TempDir,
    pub db: Db,
}

pub fn git(root: &Path, args: &[&str]) {
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

pub fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().expect("fixture paths have a parent")).unwrap();
    std::fs::write(p, content).unwrap();
}

/// A repo with one commit per entry of `commits`, each writing its files.
pub fn repo(commits: &[&[(&str, &str)]]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    write(
        r,
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    write(r, "pubspec.yaml", "name: demo\n");
    git(r, &["init", "-q"]);
    git(r, &["config", "user.email", "test@example.com"]);
    git(r, &["config", "user.name", "Test"]);
    for (i, files) in commits.iter().enumerate() {
        for (p, c) in *files {
            write(r, p, c);
        }
        git(r, &["add", "-A"]);
        git(r, &["commit", "-q", "--no-verify", "-m", &format!("c{i}")]);
    }
    root
}

/// Parse the worktree of `root` into a fresh index for workspace `id`.
pub fn index(root: tempfile::TempDir, id: &str) -> Fixture {
    let r = root.path();
    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: id.to_string(),
        root: PathBuf::from(r),
        languages: vec!["rust".to_string(), "dart".to_string()],
        frozen: false,
    };
    let config = Config {
        db_dir: db_dir.path().to_path_buf(),
        workspaces_path: db_dir.path().join("workspaces.toml"),
        listen_addr: "127.0.0.1:0".to_string(),
        parse_parallelism: 1,
        log_level: "warn".to_string(),
        constraints_idle_timeout_sec: 1800,
        parse_timeout_ms: 5000,
    };
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    pipeline::parse_workspace(
        &ws,
        &db,
        &config,
        &AtomicBool::new(false),
        &default_registry(),
    )
    .unwrap();
    Fixture {
        root,
        _db_dir: db_dir,
        db,
    }
}
