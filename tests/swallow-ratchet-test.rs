//! The swallowed-error ratchet's review side (sutra/486): forbidden-pattern
//! findings on a diff are attributed to added lines only, review and check list
//! the justifications a diff adds, and every surface records what it flagged in
//! the shared firing log.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use sutra::config::Config;
use sutra::db::Db;
use sutra::parser::adapter::default_registry;
use sutra::pipeline;
use sutra::rules::Severity;
use sutra::tools::check::{self, CheckReport};
use sutra::tools::firings::PATTERN_MECHANISM;
use sutra::tools::review;
use sutra::workspace::WorkspaceEntry;

struct Fixture {
    _root: tempfile::TempDir,
    db_dir: tempfile::TempDir,
    ws: WorkspaceEntry,
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

/// Tier-A `rs-ok`, as adopted from vidhi's rust.toml.
const RULES: &str = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
name = "rs-ok"
severity = "blocking"
justify = "swallow:"
query = '(call_expression function: (field_expression field: (field_identifier) @match (#eq? @match "ok")) arguments: (arguments) @a (#eq? @a "()"))'
"#;

const SEED: &str = "pub fn old(s: &str) -> Option<u8> {\n    s.parse().ok()\n}\n";

/// The seed plus one unjustified and one justified `.ok()`, both added.
const EDITED: &str = "pub fn old(s: &str) -> Option<u8> {\n    s.parse().ok()\n}\n\
pub fn added(s: &str) -> Option<u8> {\n    s.parse().ok()\n}\n\
pub fn justified(s: &str) -> Option<u8> {\n    // swallow: a malformed value means unset\n    s.parse().ok()\n}\n";

/// A committed crate whose `src/lib.rs` already holds one `.ok()`.
fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let r = root.path();
    write(
        r,
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
    );
    write(r, "src/lib.rs", SEED);
    write(r, ".sutra/rules.toml", RULES);
    git(r, &["init", "-q"]);
    git(r, &["config", "user.email", "test@example.com"]);
    git(r, &["config", "user.name", "Test"]);
    git(r, &["add", "-A"]);
    git(r, &["commit", "-q", "--no-verify", "-m", "seed"]);

    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: "swallow".to_string(),
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
    Fixture {
        _root: root,
        db_dir,
        ws,
        db,
    }
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

/// `(surface, finding_kind, line, snippet)` of every pattern firing.
fn pattern_firings(fx: &Fixture) -> Vec<(String, String, Option<i64>, Option<String>)> {
    fx.db
        .firings(Some(PATTERN_MECHANISM), None)
        .unwrap()
        .into_iter()
        .map(|r| (r.surface, r.finding_kind, r.line, r.snippet))
        .collect()
}

#[test]
fn check_attributes_added_lines_and_lists_justifications() {
    let fx = fixture();
    write(&fx.ws.root, "src/lib.rs", EDITED);
    git(&fx.ws.root, &["add", "-A"]);

    let report = run_check(&fx, "staged");

    let blocking: Vec<Option<u32>> = report.blocking.iter().map(|f| f.line).collect();
    assert_eq!(
        blocking,
        vec![Some(5)],
        "the old .ok() on line 2 is backlog"
    );
    let justified: Vec<(Option<u32>, &str)> = review::justified(&report.waived)
        .map(|w| (w.finding.line, w.rationale.as_str()))
        .collect();
    assert_eq!(justified, vec![(Some(9), "a malformed value means unset")]);
    let human = check::render_human(&report);
    assert!(
        human.contains("src/lib.rs:9  rs-ok: a malformed value means unset"),
        "{human}"
    );
    assert_eq!(check::to_json(&report)["justified"][0]["line"], 9);

    assert_eq!(report.firing_log_error, None);
    assert_eq!(
        pattern_firings(&fx),
        vec![(
            "check".to_string(),
            "rs-ok".to_string(),
            Some(5),
            Some("s.parse().ok()".to_string())
        )]
    );
    // Checking the same diff again is the same event and the same site.
    run_check(&fx, "staged");
    assert_eq!(pattern_firings(&fx).len(), 1);
}

#[test]
fn check_on_an_edit_that_adds_no_match_reports_nothing() {
    let fx = fixture();
    write(
        &fx.ws.root,
        "src/lib.rs",
        &format!("{SEED}pub fn other() {{}}\n"),
    );
    git(&fx.ws.root, &["add", "-A"]);
    let report = run_check(&fx, "staged");
    assert!(
        report.blocking.is_empty(),
        "the touched file's old .ok() is not the diff's: {:?}",
        report.blocking
    );
    assert!(pattern_firings(&fx).is_empty());
}

#[test]
fn review_reads_the_worktree_and_lists_justifications() {
    let fx = fixture();
    write(&fx.ws.root, "src/lib.rs", EDITED);

    let out = review::handle(&fx.db, &fx.ws.root, Some("unstaged"), None, false).unwrap();

    let lines: Vec<&serde_json::Value> = out["constraint_violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| &v["line"])
        .collect();
    assert_eq!(lines, vec![&serde_json::json!(5)]);
    assert_eq!(
        out["justified"],
        serde_json::json!([{
            "rule": "rs-ok",
            "file": "src/lib.rs",
            "line": 9,
            "snippet": "ok",
            "enclosing_symbol": "justified",
            "reason": "a malformed value means unset",
        }])
    );
    assert!(out.get("constraint_firing_log_error").is_none(), "{out}");
    assert_eq!(
        pattern_firings(&fx),
        vec![(
            "review".to_string(),
            "rs-ok".to_string(),
            Some(5),
            Some("s.parse().ok()".to_string())
        )]
    );
}

#[test]
fn guard_blocks_are_recorded_once_per_proposed_edit() {
    let fx = fixture();
    let db_path = fx.db_dir.path().join(&fx.ws.id).join("index.db");
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let outcome = sutra::guard::check_proposed_patterns(&conn, &fx.ws.root, "src/lib.rs", EDITED);
    let blocked: Vec<&sutra::constraints::ConstraintFinding> = outcome
        .active
        .iter()
        .filter(|f| f.severity == Severity::Blocking)
        .collect();
    assert_eq!(blocked.len(), 1);

    let deny = sutra::guard::format_pattern_deny(&blocked);
    assert!(deny.contains("`swallow: <reason>`"), "{deny}");
    assert!(!deny.contains("action=waive"), "{deny}");

    let record = || {
        sutra::tools::firings::record_guard_blocks(
            &conn,
            &fx.ws.root,
            "src/lib.rs",
            (SEED, EDITED),
            &blocked,
            &default_registry(),
        )
        .unwrap()
    };
    assert_eq!(record(), 1);
    assert_eq!(record(), 0, "retrying the same edit is the same site");
    assert_eq!(
        pattern_firings(&fx),
        vec![(
            "guard".to_string(),
            "rs-ok".to_string(),
            Some(5),
            Some("s.parse().ok()".to_string())
        )]
    );
}
