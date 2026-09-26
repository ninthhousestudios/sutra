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
use sutra::rules::Severity;
use sutra::tools::review;
use sutra::tools::sibling_pattern::{
    self, Budget, Controls, IdiomKind, PatternClass, SiblingReport,
};
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

/// A repo with one commit per entry of `commits`, each writing its files.
fn repo(commits: &[&[(&str, &str)]]) -> tempfile::TempDir {
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

/// Parse the worktree of `root` into a fresh index.
fn index(root: tempfile::TempDir) -> Fixture {
    let r = root.path();
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

/// Seed commit with `before` files, second commit applying `after`, index
/// parsed at the second commit.
fn fixture(before: &[(&str, &str)], after: &[(&str, &str)]) -> Fixture {
    index(repo(&[before, after]))
}

fn analyze_diff(fx: &Fixture, diff: &str) -> SiblingReport {
    analyze_with(fx, diff, Controls::default())
}

fn analyze_with(fx: &Fixture, diff: &str, controls: Controls) -> SiblingReport {
    let scope = review::resolve_diff_entries(fx.root.path(), diff).unwrap();
    sibling_pattern::analyze(
        &fx.db,
        fx.root.path(),
        &scope,
        &default_registry(),
        controls,
        Budget::default(),
    )
    .unwrap()
}

fn survivor_sites(report: &SiblingReport) -> Vec<(String, usize, Option<String>)> {
    report
        .findings
        .iter()
        .flat_map(|f| &f.survivors)
        .map(|s| {
            let symbol = s
                .symbol
                .as_deref()
                .and_then(|n| n.rsplit("::").next())
                .map(str::to_string);
            (s.file.to_string(), s.line, symbol)
        })
        .collect()
}

fn analyze(fx: &Fixture, controls: Controls) -> SiblingReport {
    analyze_with(fx, "HEAD", controls)
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

const SIBLING_SITE: (&str, usize, &str) = ("src/b.rs", 2, "load_tags");

fn expected_sibling() -> Vec<(String, usize, Option<String>)> {
    let (file, line, symbol) = SIBLING_SITE;
    vec![(file.to_string(), line, Some(symbol.to_string()))]
}

/// A staged review reads the index for every file, not only the ones the diff
/// touched: an unstaged fix to the sibling doesn't hide it, and the index
/// (parsed from the worktree) doesn't decide what survives (sutra/492).
#[test]
fn staged_review_ignores_unstaged_edits_elsewhere() {
    let root = repo(&[&[
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        ("src/a.rs", RUST_SWALLOW),
        ("src/b.rs", RUST_SIBLING),
    ]]);
    let r = root.path();
    write(r, "src/a.rs", RUST_TYPED);
    git(r, &["add", "src/a.rs"]);
    // Unstaged: the sibling fixed, and shifted, in the worktree only.
    write(
        r,
        "src/b.rs",
        "// fixed\n\npub fn load_tags(raw: &str) -> Result<Vec<String>, serde_json::Error> {\n    serde_json::from_str(raw)\n}\n",
    );
    let fx = index(root);

    let report = analyze_diff(&fx, "staged");
    assert!(report.incomplete.is_empty(), "{:?}", report.incomplete);
    assert_eq!(survivor_sites(&report), expected_sibling());
}

/// Reviewing an old commit reads that commit's tree: a survivor fixed since is
/// still listed (with its symbol as of then), one added since is not.
#[test]
fn historical_review_reads_the_commit_without_checking_it_out() {
    let root = repo(&[
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\n"),
            ("src/a.rs", RUST_SWALLOW),
            ("src/b.rs", RUST_SIBLING),
            ("src/c.rs", "pub fn other() {}\n"),
        ],
        &[("src/a.rs", RUST_TYPED)],
        &[
            ("src/b.rs", "pub fn renamed() {}\n"),
            (
                "src/c.rs",
                "pub fn load_notes(raw: &str) -> Vec<String> {\n    serde_json::from_str(raw).unwrap_or_default()\n}\n",
            ),
        ],
    ]);
    let fx = index(root);

    let report = analyze_diff(&fx, "HEAD~1");
    assert!(report.incomplete.is_empty(), "{:?}", report.incomplete);
    assert_eq!(survivor_sites(&report), expected_sibling());
}

/// Sibling findings are advisory: with or without one, `sutra check` gates the
/// same and `sutra_review` scores the same risk.
#[test]
fn sibling_findings_never_gate_or_score() {
    let base = [
        ("src/lib.rs", "pub mod a;\npub mod b;\n"),
        ("src/a.rs", RUST_SWALLOW),
    ];
    let with = fixture(
        &[base[0], base[1], ("src/b.rs", RUST_SIBLING)],
        &[("src/a.rs", RUST_TYPED)],
    );
    let without = fixture(
        &[
            base[0],
            base[1],
            (
                "src/b.rs",
                "pub fn load_tags(raw: &str) -> Vec<String> {\n    raw.lines().map(str::to_string).collect()\n}\n",
            ),
        ],
        &[("src/a.rs", RUST_TYPED)],
    );
    let registry = default_registry();

    let gate = |fx: &Fixture| {
        let report = sutra::tools::check::handle(
            &fx.db,
            fx.root.path(),
            "HEAD",
            Severity::Advisory,
            &registry,
        )
        .unwrap();
        let findings = report
            .sibling_patterns
            .as_ref()
            .map_or(0, |a| a.report.findings.len());
        (report.failed(), report.blocking.len(), findings)
    };
    let (failed_with, blocking_with, findings_with) = gate(&with);
    let (failed_without, blocking_without, findings_without) = gate(&without);
    assert_eq!((findings_with, findings_without), (1, 0));
    assert!(!failed_with, "a sibling finding must not fail the gate");
    assert_eq!(
        (failed_with, blocking_with),
        (failed_without, blocking_without)
    );

    let review = |fx: &Fixture| {
        let out = review::handle(&fx.db, fx.root.path(), Some("HEAD"), None, false).unwrap();
        let findings = out["sibling_patterns"]["findings"]
            .as_array()
            .map_or(0, Vec::len);
        (
            out["risk_score"].clone(),
            out["risk_breakdown"].clone(),
            findings,
        )
    };
    let (risk_with, breakdown_with, findings_with) = review(&with);
    let (risk_without, breakdown_without, findings_without) = review(&without);
    assert_eq!((findings_with, findings_without), (1, 0));
    assert!(risk_with.is_number(), "{risk_with}");
    assert_eq!(risk_with, risk_without);
    assert_eq!(breakdown_with, breakdown_without);
}

/// An unstaged review reads only the files the index says hold an idiom's
/// literals (sutra/494). The probe: `lib/c.dart` holds the old list on disk,
/// but the index was parsed while it didn't, so it is never read.
#[test]
fn unstaged_review_narrows_literal_idioms_by_the_index() {
    let root = repo(&[&[
        ("lib/a.dart", DART_LIST),
        ("lib/b.dart", DART_SIBLING),
        ("lib/c.dart", DART_SIBLING),
    ]]);
    let r = root.path();
    write(r, "lib/a.dart", DART_GROWN);
    write(r, "lib/c.dart", "bool other() => true;\n");
    let fx = index(root);
    git(fx.root.path(), &["checkout", "--", "lib/c.dart"]);

    let report = analyze_diff(&fx, "unstaged");
    assert!(report.incomplete.is_empty(), "{:?}", report.incomplete);
    let sites: Vec<(String, usize)> = survivor_sites(&report)
        .into_iter()
        .map(|(f, l, _)| (f, l))
        .collect();
    assert_eq!(sites, vec![("lib/b.dart".to_string(), 2)]);
}

/// A survivor scan that runs out of budget says so, and never reads as clean.
#[test]
fn exhausted_budget_is_incomplete_not_clean() {
    let fx = fixture(
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\n"),
            ("src/a.rs", RUST_SWALLOW),
            ("src/b.rs", RUST_SIBLING),
        ],
        &[("src/a.rs", RUST_TYPED)],
    );
    let scope = review::resolve_diff_entries(fx.root.path(), "HEAD").unwrap();
    let report = sibling_pattern::analyze(
        &fx.db,
        fx.root.path(),
        &scope,
        &default_registry(),
        Controls::default(),
        Budget {
            scan_time: std::time::Duration::ZERO,
        },
    )
    .unwrap();
    assert!(
        report.incomplete.iter().any(|i| i.starts_with("budget:")),
        "{:?}",
        report.incomplete
    );
}

/// A symbol moved whole to another file is not a removal site, even when the
/// same diff rewrites the idiom elsewhere (classify_symbols + resolve_renames).
#[test]
fn moved_symbol_is_not_a_removal_site() {
    let fx = fixture(
        &[
            (
                "src/lib.rs",
                "pub mod a;\npub mod b;\npub mod c;\npub mod d;\n",
            ),
            ("src/a.rs", RUST_SWALLOW),
            ("src/b.rs", RUST_SIBLING),
            ("src/c.rs", "pub fn other() {}\n"),
            (
                "src/d.rs",
                "pub fn load_notes(raw: &str) -> Vec<String> {\n    serde_json::from_str(raw).unwrap_or_default()\n}\n",
            ),
        ],
        &[
            ("src/a.rs", "pub fn unrelated() {}\n"),
            (
                "src/c.rs",
                &format!("pub fn other() {{}}\n\n{RUST_SWALLOW}"),
            ),
            (
                "src/d.rs",
                "pub fn load_notes(raw: &str) -> Result<Vec<String>, serde_json::Error> {\n    serde_json::from_str(raw)\n}\n",
            ),
        ],
    );
    let report = analyze(&fx, Controls::default());
    let chain = report
        .findings
        .iter()
        .find(|f| f.idioms.iter().any(|i| i.kind == IdiomKind::Chain))
        .expect("the rewrite in d.rs is reported");
    assert_eq!(chain.removed_at, vec!["src/d.rs:2".to_string()]);
    assert_eq!(
        survivor_sites(&report)
            .into_iter()
            .filter(|(f, _, _)| f == "src/b.rs")
            .count(),
        1,
        "{:#?}",
        report.findings
    );
}
