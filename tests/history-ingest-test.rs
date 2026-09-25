//! Real-path regression for git history ingestion (`sutra::history`): what the
//! full and unchanged parses write to `commits`/`commit_files`, when prior rows
//! are retained or cleared, and whether the ingestion reports history loaded.

use std::path::{Path, PathBuf};
use std::process::Command;

use sutra::config::Config;
use sutra::db::Db;
use sutra::git::head_commit;
use sutra::parser::adapter::default_registry;
use sutra::pipeline;
use sutra::workspace::WorkspaceEntry;

fn make_config(db_dir: &Path) -> Config {
    Config {
        db_dir: db_dir.to_path_buf(),
        workspaces_path: db_dir.join("workspaces.toml"),
        listen_addr: "127.0.0.1:0".to_string(),
        parse_parallelism: 1,
        log_level: "warn".to_string(),
        constraints_idle_timeout_sec: 1800,
        parse_timeout_ms: 5000,
    }
}

fn make_entry(id: &str, root: PathBuf) -> WorkspaceEntry {
    WorkspaceEntry {
        id: id.to_string(),
        root,
        languages: vec!["rust".to_string()],
        frozen: false,
    }
}

/// A fixture: a workspace with the given files, a config and an open db. The two
/// tempdirs are returned so they outlive the test body.
struct Fixture {
    _root: tempfile::TempDir,
    _db_dir: tempfile::TempDir,
    ws: WorkspaceEntry,
    config: Config,
    db: Db,
}

fn fixture(id: &str, files: &[(&str, &str)]) -> Fixture {
    let root = tempfile::tempdir().unwrap();
    for (rel, contents) in files {
        let full = root.path().join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, contents).unwrap();
    }
    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry(id, root.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    Fixture {
        _root: root,
        _db_dir: db_dir,
        ws,
        config,
        db,
    }
}

fn full_parse(fx: &Fixture) -> pipeline::ParseSnapshot {
    let cancel = std::sync::atomic::AtomicBool::new(false);
    let registry = default_registry();
    pipeline::parse_workspace(&fx.ws, &fx.db, &fx.config, &cancel, &registry).unwrap()
}

fn commit_file_rows(db: &Db) -> i64 {
    db.conn_for_test()
        .query_row("SELECT COUNT(*) FROM commit_files", [], |r| r.get(0))
        .unwrap()
}

/// Whether an ingestion against the fixture's current repository state reports
/// loaded history. Ingestion is idempotent for a fixed repository and day.
fn history_loaded(fx: &Fixture) -> bool {
    sutra::history::ingest(&fx.db, &fx.ws.root, chrono::Utc::now().timestamp())
        .unwrap()
        .loaded
}

/// File pairs whose co-change Jaccard reaches 0.5 — the review
/// `behavioral_coupling` and clustering consumer of ingested history.
fn cochange_pair_count(db: &Db) -> usize {
    db.cochange_pairs_above_threshold(0.5).unwrap().len()
}

// --- git fixture helpers ---

/// Run a git command in `root`, optionally pinning committer+author date to a
/// fixed unix timestamp, and assert success.
fn git(root: &Path, args: &[&str], date: Option<i64>) {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    if let Some(ts) = date {
        let val = format!("@{ts} +0000");
        cmd.env("GIT_COMMITTER_DATE", &val)
            .env("GIT_AUTHOR_DATE", &val);
    }
    let out = cmd.output().expect("git spawn");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_init(root: &Path) {
    git(root, &["init", "-q"], None);
    git(root, &["config", "user.email", "test@example.com"], None);
    git(root, &["config", "user.name", "Test"], None);
    // Deterministic default branch regardless of the host git's init.defaultBranch.
    git(root, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
}

/// Stage everything and commit with a pinned committer timestamp.
fn git_commit(root: &Path, ts: i64) {
    git(root, &["add", "-A"], None);
    // --no-verify: skip any inherited pre-commit hook (e.g. a global rustfmt
    // gate) — these seed files are fixtures, not project source.
    git(
        root,
        &["commit", "-q", "--no-verify", "-m", "seed"],
        Some(ts),
    );
}

/// A `fixture` whose workspace root is a real git repo with the given files
/// committed in two passes, each touching every file, so any indexed pair
/// co-changes with jaccard 1.0. Committer timestamps sit comfortably inside the
/// trailing 90-day window, so history ingests as `Loaded`.
fn git_fixture(id: &str, files: &[(&str, &str)]) -> Fixture {
    let fx = fixture(id, files);
    git_init(&fx.ws.root);
    let now = chrono::Utc::now().timestamp();
    git_commit(&fx.ws.root, now - 7200);
    // A second pass touching every file, so the history is plural and the
    // co-change signal survives the eligible-commit fan-out filter cleanly.
    for (rel, _) in files {
        let p = fx.ws.root.join(rel);
        let mut src = std::fs::read_to_string(&p).unwrap();
        src.push_str("// seed touch\n");
        std::fs::write(&p, src).unwrap();
    }
    git_commit(&fx.ws.root, now - 3600);
    fx
}

/// The pair of indexed files every git test co-changes. They start with no
/// cross-file reference, so the co-change lacks a static edge → HiddenCoupling
/// fires. Mirrors the resolving import construct proven by the rollups test.
const GIT_FILES: [(&str, &str); 2] = [
    ("src/lib.rs", "pub fn hello() -> i32 { 1 }\n"),
    ("src/util.rs", "pub fn greet() -> i32 { 2 }\n"),
];

/// Build a workspace that is a *shallow* clone (`--depth 1`) of a three-commit
/// origin repo. Returns the fixture plus the origin tempdir the caller must keep
/// alive. A shallow clone's window cannot be established complete (sutra/427).
fn shallow_git_fixture(id: &str) -> (Fixture, tempfile::TempDir) {
    let origin = tempfile::tempdir().unwrap();
    for (rel, contents) in GIT_FILES {
        let full = origin.path().join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, contents).unwrap();
    }
    git_init(origin.path());
    let now = chrono::Utc::now().timestamp();
    // Three commits, each touching every file, all inside the trailing window.
    for (i, ts) in [now - 10_800, now - 7_200, now - 3_600]
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            for (rel, _) in GIT_FILES {
                let p = origin.path().join(rel);
                let mut src = std::fs::read_to_string(&p).unwrap();
                src.push_str("// seed touch\n");
                std::fs::write(&p, src).unwrap();
            }
        }
        git_commit(origin.path(), ts);
    }

    // A depth-1 clone of the origin over file:// (a plain local path ignores
    // --depth): the working tree checks out, the ancestry is truncated.
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    let origin_url = format!("file://{}", origin.path().display());
    git(
        root.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &origin_url,
            work.to_str().unwrap(),
        ],
        None,
    );

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry(id, work);
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    (
        Fixture {
            _root: root,
            _db_dir: db_dir,
            ws,
            config,
            db,
        },
        origin,
    )
}

#[test]
fn full_parse_with_history_ingests_and_feeds_cochange() {
    let fx = git_fixture("git-loaded", &GIT_FILES);
    full_parse(&fx);

    assert!(
        commit_file_rows(&fx.db) > 0,
        "indexed files have in-window commits"
    );
    assert!(
        cochange_pair_count(&fx.db) > 0,
        "co-changing lib/util must yield a co-change pair"
    );
    assert!(history_loaded(&fx));
}

#[test]
fn shallow_clone_ingests_nothing() {
    let (fx, _origin) = shallow_git_fixture("git-shallow");
    full_parse(&fx);

    assert!(head_commit(&fx.ws.root).unwrap().is_some());
    assert_eq!(
        commit_file_rows(&fx.db),
        0,
        "a truncated window is never ingested as complete history"
    );
    assert!(!history_loaded(&fx));
}

/// Break `git log` without touching the repository probe: drop the loose tree
/// object of HEAD's parent. `rev-parse HEAD`, the repo identity and the shallow
/// probe still succeed (the repo is Present at an unchanged HEAD), but the
/// `--name-only` diff against the parent fails with `unable to read tree`.
fn corrupt_parent_tree(root: &Path) {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD~1^{tree}"])
        .output()
        .expect("git spawn");
    assert!(out.status.success(), "parent tree must resolve");
    let tree = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let object = root.join(".git/objects").join(&tree[..2]).join(&tree[2..]);
    std::fs::remove_file(&object).expect("parent tree is a loose object");
}

#[test]
fn git_log_failure_retains_commit_files_and_is_not_loaded() {
    let fx = git_fixture("git-failure", &GIT_FILES);
    full_parse(&fx);
    let rows_before = commit_file_rows(&fx.db);
    assert!(
        rows_before > 0,
        "baseline ingestion must populate commit_files"
    );
    assert!(cochange_pair_count(&fx.db) > 0);

    corrupt_parent_tree(&fx.ws.root);
    // A comment edit forces the next full parse through history ingestion rather
    // than the no-change path.
    let mut src = std::fs::read_to_string(fx.ws.root.join("src/util.rs")).unwrap();
    src.push_str("// trailing comment\n");
    std::fs::write(fx.ws.root.join("src/util.rs"), &src).unwrap();
    let snap = full_parse(&fx);
    assert_eq!(snap.files_parsed, 1, "the edited file reparses");
    assert!(
        head_commit(&fx.ws.root).unwrap().is_some(),
        "the repo is still present at a resolved HEAD"
    );

    assert_eq!(
        commit_file_rows(&fx.db),
        rows_before,
        "a transient git failure must not clear commit_files"
    );
    assert!(
        !history_loaded(&fx),
        "retained rows are not reported as loaded history"
    );
}

#[test]
fn non_repository_has_no_history() {
    let fx = fixture("not-a-repo", &GIT_FILES);
    full_parse(&fx);
    assert_eq!(commit_file_rows(&fx.db), 0);
    assert!(!history_loaded(&fx));
}

#[test]
fn history_touching_only_unindexed_paths_is_not_loaded() {
    let fx = fixture("unindexed-history", &[("src/lib.rs", "pub fn f() {}\n")]);
    git_init(&fx.ws.root);
    // The only in-window commit touches a non-indexed path.
    std::fs::write(fx.ws.root.join("README.md"), "readme\n").unwrap();
    git(&fx.ws.root, &["add", "README.md"], None);
    git(
        &fx.ws.root,
        &["commit", "-q", "--no-verify", "-m", "readme"],
        Some(chrono::Utc::now().timestamp() - 3600),
    );

    full_parse(&fx);

    assert_eq!(commit_file_rows(&fx.db), 0);
    assert!(!history_loaded(&fx));
}

#[test]
fn unchanged_parse_ingests_a_new_commit() {
    let fx = git_fixture("nochange-ingest", &GIT_FILES);
    full_parse(&fx);
    let newest_before = fx.db.newest_commit_at().unwrap();

    // A commit touching only an unindexed file: HEAD moves, no indexed source
    // changes, so the next parse takes the no-change path.
    std::fs::write(fx.ws.root.join("NOTES.md"), "notes\n").unwrap();
    git_commit(&fx.ws.root, chrono::Utc::now().timestamp() - 60);
    let snap = full_parse(&fx);
    assert_eq!(snap.files_parsed, 0, "no indexed source changed");

    assert!(
        fx.db.newest_commit_at().unwrap() > newest_before,
        "the unchanged parse re-ingested history at the new HEAD"
    );
}

// --- component membership across history moves ---

/// Two groups of three mutually-calling files, each group committed on its own
/// (twice) so both static edges and co-change stay within a group: clustering
/// yields multi-member components.
fn two_clique_fixture(id: &str) -> Fixture {
    let mut files: Vec<(String, String)> = vec![(
        "src/lib.rs".into(),
        "pub mod alpha;\npub mod beta;\n".into(),
    )];
    for group in ["alpha", "beta"] {
        files.push((
            format!("src/{group}/mod.rs"),
            "pub mod a;\npub mod b;\npub mod c;\n".into(),
        ));
        for (me, x, y) in [("a", "b", "c"), ("b", "c", "a"), ("c", "a", "b")] {
            let body = format!("pub fn {group}_{me}(x: i32) -> i32 {{\n    x\n}}\n");
            files.push((
                format!("src/{group}/{me}.rs"),
                format!(
                    "use super::{x}::{group}_{x};\nuse super::{y}::{group}_{y};\n\n\
                     pub fn {group}_{me}_calls(v: i32) -> i32 {{\n    {group}_{x}(v) + {group}_{y}(v)\n}}\n\n{body}"
                ),
            ));
        }
    }
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();
    let fx = fixture(id, &refs);
    git_init(&fx.ws.root);
    let now = chrono::Utc::now().timestamp();
    git(&fx.ws.root, &["add", "src/lib.rs"], None);
    for (i, group) in ["alpha", "beta"].into_iter().enumerate() {
        for pass in 0..2 {
            if pass > 0 {
                for (rel, _) in files.iter().filter(|(p, _)| p.contains(group)) {
                    let p = fx.ws.root.join(rel);
                    let mut src = std::fs::read_to_string(&p).unwrap();
                    src.push_str("// seed touch\n");
                    std::fs::write(&p, src).unwrap();
                }
            }
            git(&fx.ws.root, &["add", &format!("src/{group}")], None);
            git(
                &fx.ws.root,
                &["commit", "-q", "--no-verify", "-m", "seed"],
                Some(now - 7200 + (i as i64 * 2 + pass) * 600),
            );
        }
    }
    fx
}

/// `(component id, member file id)` pairs of the live membership, sorted. File
/// ids are stable across content edits (sutra/413).
fn membership(db: &Db) -> Vec<(String, i64)> {
    let mut out: Vec<(String, i64)> = db
        .component_members_with_line_count()
        .unwrap()
        .into_iter()
        .map(|(comp, fid, _)| (comp, fid))
        .collect();
    out.sort();
    out
}

#[test]
fn comment_only_edit_keeps_the_file_in_its_component() {
    let fx = two_clique_fixture("component-keep");
    full_parse(&fx);
    let before = membership(&fx.db);
    let edited = fx.db.file_by_path("src/alpha/b.rs").unwrap().unwrap().id;
    assert!(
        before.iter().any(|&(_, fid)| fid == edited),
        "fixture clusters the edited file: {before:?}"
    );

    let path = fx.ws.root.join("src/alpha/b.rs");
    let mut src = std::fs::read_to_string(&path).unwrap();
    src.push_str("// comment only\n");
    std::fs::write(&path, &src).unwrap();
    full_parse(&fx);

    // The full parse does not re-cluster (edge drift is under threshold), so the
    // content edit must not have deleted the file's membership either (sutra/439).
    assert_eq!(membership(&fx.db), before);
    assert!(sutra::components::membership_current(&fx.db, &fx.ws.root).unwrap());
}

#[test]
fn unchanged_parse_after_a_new_commit_re_clusters_stale_membership() {
    // sutra/443: a commit touching only an unindexed file moves the newest
    // ingested commit, which stales the clustering. The next parse sees no source
    // change (NoChanges) and must still re-cluster.
    let fx = two_clique_fixture("nochange-recluster");
    full_parse(&fx);
    let before = membership(&fx.db);
    assert!(!before.is_empty(), "fixture yields components");

    std::fs::write(fx.ws.root.join("NOTES.md"), "notes\n").unwrap();
    git_commit(&fx.ws.root, chrono::Utc::now().timestamp() - 60);
    let snap = full_parse(&fx);
    assert_eq!(snap.files_parsed, 0, "no indexed source changed");

    assert!(
        sutra::components::membership_current(&fx.db, &fx.ws.root).unwrap(),
        "the unchanged parse re-clustered against the new history"
    );
    assert_eq!(
        membership(&fx.db),
        before,
        "same history shape, same components"
    );
}
