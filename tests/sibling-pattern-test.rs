//! The "you fixed 1 of N" advisory end to end (sutra/467): a commit rewrites
//! an idiom at one site, the index-backed search finds the sibling that still
//! has it, and the firing log records it once per diff.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use sutra::config::Config;
use sutra::db::Db;
use sutra::parser::adapter::default_registry;
use sutra::pipeline;
use sutra::tools::review;
use sutra::tools::sibling_pattern::{self, Controls, IdiomKind, PatternClass, SiblingReport};
use sutra::workspace::WorkspaceEntry;

struct Fixture {
    root: tempfile::TempDir,
    _db_dir: tempfile::TempDir,
    db: Db,
}

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

fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().expect("fixture paths have a parent")).unwrap();
    std::fs::write(p, content).unwrap();
}

const RUST_SWALLOW: &str = "pub fn load_refs(raw: &str) -> Vec<String> {\n    serde_json::from_str(raw).unwrap_or_default()\n}\n";
const RUST_TYPED: &str = "pub fn load_refs(raw: &str) -> Result<Vec<String>, serde_json::Error> {\n    serde_json::from_str(raw)\n}\n";
const RUST_SIBLING: &str = "pub fn load_tags(raw: &str) -> Vec<String> {\n    serde_json::from_str(raw).unwrap_or_default()\n}\n";

const DART_LIST: &str =
    "const kinds = ['draft', 'final'];\n\nbool known(String k) {\n  return kinds.contains(k);\n}\n";
const DART_GROWN: &str = "const kinds = ['draft', 'final', 'archived'];\n\nbool known(String k) {\n  return kinds.contains(k);\n}\n";
const DART_SIBLING: &str =
    "bool editable(String k) {\n  return ['draft', 'final'].contains(k);\n}\n";

/// Seed commit with `before` files, second commit applying `after`, index
/// parsed at the second commit.
fn fixture(before: &[(&str, &str)], after: &[(&str, &str)]) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    write(
        r,
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    write(r, "pubspec.yaml", "name: demo\n");
    for (p, c) in before {
        write(r, p, c);
    }
    git(r, &["init", "-q"]);
    git(r, &["config", "user.email", "test@example.com"]);
    git(r, &["config", "user.name", "Test"]);
    git(r, &["add", "-A"]);
    git(r, &["commit", "-q", "--no-verify", "-m", "seed"]);
    for (p, c) in after {
        write(r, p, c);
    }
    git(r, &["add", "-A"]);
    git(r, &["commit", "-q", "--no-verify", "-m", "rewrite"]);

    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: "sibling".to_string(),
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

fn analyze(fx: &Fixture, controls: Controls) -> SiblingReport {
    let scope = review::resolve_diff_entries(fx.root.path(), "HEAD").unwrap();
    sibling_pattern::analyze(
        &fx.db,
        fx.root.path(),
        &scope,
        &default_registry(),
        controls,
    )
    .unwrap()
}

#[test]
fn rust_rewrite_lists_the_surviving_sibling_with_its_symbol() {
    let fx = fixture(
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", RUST_SWALLOW),
            ("src/b.rs", RUST_SIBLING),
        ],
        &[("src/a.rs", RUST_TYPED)],
    );
    let report = analyze(&fx, Controls::default());
    assert!(report.incomplete.is_empty(), "{:?}", report.incomplete);
    assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.class, PatternClass::Rewritten);
    assert_eq!(f.idioms[0].kind, IdiomKind::Chain);
    assert_eq!(f.idioms[0].idiom, "serde_json::from_str.unwrap_or_default");
    assert_eq!(f.removed_at, vec!["src/a.rs:2".to_string()]);
    assert_eq!(f.survivor_count, 1);
    assert_eq!(f.survivors[0].file, "src/b.rs");
    assert_eq!(f.survivors[0].line, 2);
    assert!(
        f.survivors[0]
            .symbol
            .as_deref()
            .is_some_and(|s| s.ends_with("load_tags")),
        "{:?}",
        f.survivors[0].symbol
    );
}

#[test]
fn firings_are_logged_once_per_diff() {
    let fx = fixture(
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", RUST_SWALLOW),
            ("src/b.rs", RUST_SIBLING),
        ],
        &[("src/a.rs", RUST_TYPED)],
    );
    let scope = review::resolve_diff_entries(fx.root.path(), "HEAD").unwrap();
    let registry = default_registry();
    let first =
        sibling_pattern::run_advisory(&fx.db, fx.root.path(), &scope, &registry, "check", "HEAD");
    assert!(first.error.is_none() && first.firing_log_error.is_none());
    let again =
        sibling_pattern::run_advisory(&fx.db, fx.root.path(), &scope, &registry, "check", "HEAD");
    assert!(again.firing_log_error.is_none());

    let rows = fx
        .db
        .firings(Some(sibling_pattern::MECHANISM), None)
        .unwrap();
    assert_eq!(rows.len(), 1, "a repeated review must not re-count");
    let row = &rows[0];
    assert_eq!(row.file_path, "src/b.rs");
    assert_eq!(row.line, Some(2));
    assert_eq!(row.finding_kind, "chain");
    assert_eq!(
        row.snippet.as_deref(),
        Some("serde_json::from_str(raw).unwrap_or_default()")
    );
    assert!(row.anchor_commit.is_some());

    let out = sutra::tools::firings::handle(&fx.db, fx.root.path(), None, None).unwrap();
    assert_eq!(out["firings"][0]["site_status"], "present");
    write(fx.root.path(), "src/b.rs", RUST_TYPED);
    let out = sutra::tools::firings::handle(&fx.db, fx.root.path(), None, None).unwrap();
    assert_eq!(out["firings"][0]["site_status"], "changed");
    assert_eq!(out["totals"]["sibling_pattern"]["changed"], 1);
}

#[test]
fn moved_code_is_not_a_fix() {
    let fx = fixture(
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\n"),
            ("src/a.rs", RUST_SWALLOW),
            ("src/b.rs", RUST_SIBLING),
            ("src/c.rs", "pub fn other() {}\n"),
        ],
        &[
            ("src/a.rs", "pub fn unrelated() {}\n"),
            (
                "src/c.rs",
                &format!("pub fn other() {{}}\n\n{RUST_SWALLOW}"),
            ),
        ],
    );
    let report = analyze(&fx, Controls::default());
    assert!(report.findings.is_empty(), "{:#?}", report.findings);
}

#[test]
fn dart_grown_list_is_rewritten_only_with_the_control_on() {
    let before = [("lib/a.dart", DART_LIST), ("lib/b.dart", DART_SIBLING)];
    let fx = fixture(&before, &[("lib/a.dart", DART_GROWN)]);

    let report = analyze(&fx, Controls::default());
    assert_eq!(report.findings.len(), 1, "{:#?}", report.findings);
    let f = &report.findings[0];
    assert_eq!(f.class, PatternClass::Rewritten);
    assert_eq!(f.idioms[0].kind, IdiomKind::Litset);
    assert_eq!(
        f.idioms[0].idiom,
        "{'draft', 'final'} extended by 'archived'"
    );
    assert_eq!(f.survivors[0].file, "lib/b.dart");
    assert_eq!(f.survivors[0].line, 2);

    let off = Controls {
        grown_litset: false,
        ..Controls::default()
    };
    assert!(analyze(&fx, off).findings.is_empty());
}
