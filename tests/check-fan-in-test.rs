//! `sutra check --diff` and graph-wide `max_fan_in` drift (sutra/440).
//!
//! A hub already over its fan-in threshold must block only a diff that adds
//! importers to it; pre-existing drift is reported as Informational so an
//! unrelated (even docs-only) commit passes. The gate must also see fan-in as
//! of the working tree after the query-path incremental refresh, which defers
//! the stored `fan_in_files` rollup to the next full parse.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use sutra::config::Config;
use sutra::constraints::check::FindingDelta;
use sutra::db::Db;
use sutra::parser::adapter::default_registry;
use sutra::pipeline;
use sutra::rules::Severity;
use sutra::tools::check::{self, CheckReport};
use sutra::workspace::WorkspaceEntry;

struct Fixture {
    _root: tempfile::TempDir,
    _db_dir: tempfile::TempDir,
    ws: WorkspaceEntry,
    config: Config,
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

const IMPORTER: &str = "use crate::state::State;\npub fn run(_s: State) {}\n";

/// A committed crate where `src/state.rs` has fan-in 4 (lib.rs's `mod state`
/// plus a.rs, b.rs, c.rs) against a blocking threshold of 3 — drift that
/// predates any diff under test.
fn drifted_fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    write(
        r,
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    write(
        r,
        "src/lib.rs",
        "pub mod state;\npub mod a;\npub mod b;\npub mod c;\n",
    );
    write(r, "src/state.rs", "pub struct State;\n");
    for f in ["a", "b", "c"] {
        write(r, &format!("src/{f}.rs"), IMPORTER);
    }
    write(
        r,
        ".sutra/rules.toml",
        "[[constraint]]\nkind = \"max_fan_in\"\nname = \"state-fan-in\"\n\
         target = \"src/state.rs\"\nthreshold = 3\nseverity = \"blocking\"\n",
    );
    git(r, &["init", "-q"]);
    git(r, &["config", "user.email", "test@example.com"]);
    git(r, &["config", "user.name", "Test"]);
    git(r, &["add", "-A"]);
    git(r, &["commit", "-q", "--no-verify", "-m", "seed"]);

    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: "fan-in".to_string(),
        root: PathBuf::from(r),
        languages: vec!["rust".to_string()],
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
    assert_eq!(state_stored_fan_in(&db), 4, "fixture must start drifted");
    Fixture {
        _root: root,
        _db_dir: db_dir,
        ws,
        config,
        db,
    }
}

fn state_stored_fan_in(db: &Db) -> i64 {
    db.file_by_path("src/state.rs")
        .unwrap()
        .expect("state.rs indexed")
        .fan_in_files
}

/// The `sutra check` query-path refresh: incremental reparse of the drift set.
fn refresh(fx: &Fixture) {
    let (_, drift) = sutra::freshness::workspace_drift(&fx.db, &fx.ws.root, &fx.ws.languages);
    let drift = drift.expect("baseline exists");
    pipeline::parse_incremental(&fx.ws, &fx.db, &fx.config, &default_registry(), &drift).unwrap();
}

fn run_check(fx: &Fixture, diff: &str) -> CheckReport {
    check::handle(
        &fx.db,
        &fx.ws.root,
        diff,
        Severity::Blocking,
        &default_registry(),
    )
    .unwrap()
}

fn fan_in_findings(report: &CheckReport) -> Vec<&sutra::constraints::ConstraintFinding> {
    report
        .blocking
        .iter()
        .chain(&report.below_threshold)
        .filter(|f| f.constraint_kind == "max_fan_in")
        .collect()
}

#[test]
fn docs_only_staged_diff_passes_over_preexisting_fan_in_drift() {
    let fx = drifted_fixture();
    let root = &fx.ws.root;
    write(root, "docs/notes.md", "# notes\n");
    git(root, &["add", "docs/notes.md"]);

    let report = run_check(&fx, "staged");
    assert!(
        !report.failed(),
        "a docs-only diff must not fail on graph-wide drift: {:#?}",
        report.blocking
    );
    let found = fan_in_findings(&report);
    assert_eq!(found.len(), 1, "drift stays visible: {found:#?}");
    assert_eq!(found[0].severity, Severity::Informational);
    assert_eq!(found[0].delta, FindingDelta::PreExisting);
    assert!(
        found[0].detail.contains("fan-in is 4"),
        "{}",
        found[0].detail
    );
}

#[test]
fn edit_to_existing_importer_does_not_block() {
    let fx = drifted_fixture();
    let root = &fx.ws.root;
    // Touch an importer without changing its imports.
    write(
        root,
        "src/a.rs",
        &format!("{IMPORTER}pub fn extra() {{}}\n"),
    );
    git(root, &["add", "src/a.rs"]);
    refresh(&fx);

    let report = run_check(&fx, "staged");
    assert!(!report.failed(), "{:#?}", report.blocking);
    assert_eq!(fan_in_findings(&report)[0].delta, FindingDelta::PreExisting);
}

#[test]
fn diff_adding_an_importer_blocks() {
    let fx = drifted_fixture();
    let root = &fx.ws.root;
    write(root, "src/d.rs", IMPORTER);
    write(
        root,
        "src/lib.rs",
        "pub mod state;\npub mod a;\npub mod b;\npub mod c;\npub mod d;\n",
    );
    git(root, &["add", "-A"]);
    refresh(&fx);

    let report = run_check(&fx, "staged");
    assert!(
        report.failed(),
        "a new importer of a drifted hub must block"
    );
    let found = fan_in_findings(&report);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].severity, Severity::Blocking);
    assert_eq!(found[0].delta, FindingDelta::Introduced);
    assert!(
        found[0].detail.contains("fan-in is 5"),
        "{}",
        found[0].detail
    );
}

#[test]
fn existing_file_gaining_an_import_blocks() {
    let fx = drifted_fixture();
    let root = &fx.ws.root;
    // The `use` line is what the diff adds; lib.rs already reached state.rs via
    // `mod state`, so add the import to a fresh non-importer instead.
    write(root, "src/e.rs", "pub fn e() {}\n");
    write(
        root,
        "src/lib.rs",
        "pub mod state;\npub mod a;\npub mod b;\npub mod c;\npub mod e;\n",
    );
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "--no-verify", "-m", "add e"]);
    refresh(&fx);

    write(root, "src/e.rs", IMPORTER);
    git(root, &["add", "src/e.rs"]);
    refresh(&fx);

    let report = run_check(&fx, "staged");
    assert!(report.failed(), "{:#?}", report.below_threshold);
    assert_eq!(fan_in_findings(&report)[0].delta, FindingDelta::Introduced);
}

#[test]
fn moving_an_import_between_files_is_not_attributed() {
    let fx = drifted_fixture();
    let root = &fx.ws.root;
    // a.rs drops its import, new d.rs picks one up: fan-in stays at 4, so the
    // diff hasn't grown the drift.
    write(root, "src/a.rs", "pub fn run() {}\n");
    write(root, "src/d.rs", IMPORTER);
    write(
        root,
        "src/lib.rs",
        "pub mod state;\npub mod a;\npub mod b;\npub mod c;\npub mod d;\n",
    );
    git(root, &["add", "-A"]);
    refresh(&fx);

    let report = run_check(&fx, "staged");
    assert!(!report.failed(), "{:#?}", report.blocking);
    let found = fan_in_findings(&report);
    assert_eq!(found[0].delta, FindingDelta::PreExisting);
    assert!(
        found[0].detail.contains("fan-in is 4"),
        "{}",
        found[0].detail
    );
}

#[test]
fn check_sees_working_tree_fan_in_after_incremental_refresh() {
    let fx = drifted_fixture();
    let root = &fx.ws.root;
    // Drop one importer in the worktree: fan-in falls to 3, within threshold.
    write(root, "src/a.rs", "pub fn run() {}\n");
    refresh(&fx);
    assert_eq!(
        state_stored_fan_in(&fx.db),
        4,
        "the incremental refresh defers rollups — the stored column is stale, \
         which is what the gate must not read"
    );

    let report = run_check(&fx, "unstaged");
    assert!(
        fan_in_findings(&report).is_empty(),
        "fan-in must reflect the working tree edit: {:#?}",
        report.blocking
    );
}
