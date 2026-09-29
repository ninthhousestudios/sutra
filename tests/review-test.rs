use std::fs;
use std::time::Duration;

use sutra::constraints::DdEngine;
use sutra::constraints::check::FindingDelta;
use sutra::db::{Db, InsertSymbolParams};
use sutra::parser::adapter::default_registry;
use sutra::rules::Severity;
use sutra::tools::changed_symbols::ChangedSymbols;
use sutra::tools::review;
use sutra::tools::symbol_diff::{ChangeKind, DiffFilesResult, SymbolChange};
use sutra::waivers::Waived;

/// Every line of every changed file counts as added. These tests exercise the
/// finding pipeline, not added-line attribution, and have no git history.
struct WholeFiles(sutra::constraints::check::AddedLines);

impl WholeFiles {
    fn head(&self) -> sutra::constraints::check::DiffHead<'_> {
        sutra::constraints::check::DiffHead {
            content: sutra::constraints::check::ContentSource::Worktree,
            added_lines: &self.0,
        }
    }
}

fn whole_files(changed: &[String]) -> WholeFiles {
    WholeFiles(
        changed
            .iter()
            .map(|p| (p.clone(), std::iter::once(1..usize::MAX).collect()))
            .collect(),
    )
}

fn sym<'a>(
    file_id: i64,
    qn: &'a str,
    sn: &'a str,
    sig: Option<&'a str>,
    sl: i64,
    el: i64,
    cognitive: Option<i64>,
) -> InsertSymbolParams<'a> {
    InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind: "function",
        signature: sig,
        signature_hash: None,
        structural_hash: None,
        visibility: Some("pub"),
        start_line: sl,
        start_col: 0,
        end_line: el,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive,
        flags: 0,
        language_attrs: None,
    }
}

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

fn no_findings() -> review::ReviewFindings {
    review::ReviewFindings::default()
}

/// The symbol changes of `paths` when the diff classified none.
fn changes(db: &Db, paths: &[String]) -> ChangedSymbols {
    with_changes(db, paths, Vec::new())
}

/// The symbol changes of `paths`, as the diff classified them.
fn with_changes(db: &Db, paths: &[String], per_file: Vec<(&str, SymbolChange)>) -> ChangedSymbols {
    let mut diff = DiffFilesResult {
        per_file: Default::default(),
        errors: Default::default(),
    };
    for (path, change) in per_file {
        diff.per_file
            .entry(path.to_string())
            .or_default()
            .push(change);
    }
    ChangedSymbols::from_diff(db, paths, diff).unwrap()
}

fn change(symbol: &str, change: ChangeKind) -> SymbolChange {
    SymbolChange {
        symbol: symbol.to_string(),
        kind: "function".to_string(),
        change,
        callee_diff: None,
        from_symbol: None,
        from_file: None,
    }
}

/// Fields the review dropped in sutra/517: a saturating score, blast-radius
/// ties and whole-file symbol lists.
const DROPPED: &[&str] = &[
    "risk_score",
    "risk_breakdown",
    "recommended_reads",
    "affected_files",
    "affected_symbols",
    "affected_total",
    "churn_window_days",
    "_explain",
];

// ── Structural core tests (from v1/13) ──────────────────────────────

#[test]
fn empty_diff_returns_correct_shape() {
    let (dir, db) = setup_db();
    let result = review::compute(&db, dir.path(), &[], &changes(&db, &[]), &no_findings()).unwrap();

    assert_eq!(result["changed_files"].as_array().unwrap().len(), 0);
    assert_eq!(result["changed_symbols"].as_array().unwrap().len(), 0);
    assert_eq!(result["constraint_violations"].as_array().unwrap().len(), 0);
    for field in DROPPED {
        assert!(result.get(field).is_none(), "{field} was dropped: {result}");
    }
}

fn setup_db_with_files() -> (tempfile::TempDir, Db) {
    let (dir, db) = setup_db();

    db.upsert_file("src/core.rs", "rust", "h1", 200, true)
        .unwrap();
    db.upsert_file("src/helper.rs", "rust", "h2", 50, true)
        .unwrap();
    db.upsert_file("src/consumer.rs", "rust", "h3", 100, true)
        .unwrap();

    let f_core = db.file_by_path("src/core.rs").unwrap().unwrap();
    let f_helper = db.file_by_path("src/helper.rs").unwrap().unwrap();
    let f_consumer = db.file_by_path("src/consumer.rs").unwrap().unwrap();

    db.insert_symbol(&sym(
        f_core.id,
        "core::process",
        "process",
        Some("fn process()"),
        1,
        40,
        Some(20),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        f_helper.id,
        "helper::format",
        "format",
        Some("fn format()"),
        1,
        10,
        Some(3),
    ))
    .unwrap();

    let sym_core = db.find_symbols_by_file(f_core.id).unwrap();
    db.insert_symbol(&sym(
        f_consumer.id,
        "consumer::run",
        "run",
        Some("fn run()"),
        1,
        20,
        Some(5),
    ))
    .unwrap();

    // consumer references core::process
    db.insert_ref(f_consumer.id, Some(sym_core[0].id), None, 5, 0, "call")
        .unwrap();

    // Set blast radii
    db.update_rollups(f_core.id, 2, 25).unwrap();
    db.update_rollups(f_helper.id, 0, 3).unwrap();
    db.update_rollups(f_consumer.id, 1, 5).unwrap();

    (dir, db)
}

#[test]
fn changed_symbols_are_the_diffs_changes_not_the_whole_file() {
    let (dir, db) = setup_db_with_files();
    let f_core = db.file_by_path("src/core.rs").unwrap().unwrap();
    db.insert_symbol(&sym(
        f_core.id,
        "core::untouched",
        "untouched",
        None,
        41,
        60,
        Some(9),
    ))
    .unwrap();
    let changed = vec!["src/core.rs".to_string(), "README.md".to_string()];
    let mut body = change("core::process", ChangeKind::BodyChanged);
    body.callee_diff = Some(sutra::tools::symbol_diff::CalleeDiff {
        added: vec!["validate".into()],
        removed: vec![],
    });
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &with_changes(
            &db,
            &changed,
            vec![
                ("src/core.rs", body),
                ("src/core.rs", change("core::gone", ChangeKind::Deleted)),
            ],
        ),
        &no_findings(),
    )
    .unwrap();

    let cf = result["changed_files"].as_array().unwrap();
    assert_eq!(cf.len(), 2, "every changed file is listed");
    assert_eq!(cf[0]["path"], "src/core.rs");
    assert!(cf[0].get("blast_radius").is_none());
    assert!(cf[0].get("symbol_count").is_none());
    let sc = cf[0]["symbol_changes"].as_array().unwrap();
    assert_eq!(sc.len(), 2);
    assert_eq!(sc[0]["symbol"], "core::process");
    assert_eq!(sc[0]["change"], "body_changed");
    assert_eq!(sc[0]["callee_diff"]["added"][0], "validate");
    assert_eq!(cf[1]["path"], "README.md");
    assert_eq!(cf[1]["symbol_changes"].as_array().unwrap().len(), 0);

    let cs = result["changed_symbols"].as_array().unwrap();
    let names: Vec<&str> = cs.iter().map(|s| s["symbol"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        vec!["core::process", "core::gone"],
        "core::untouched is in the file but not in the diff"
    );
    assert_eq!(cs[0]["file"], "src/core.rs");
    assert_eq!(cs[0]["kind"], "function");
    assert_eq!(cs[0]["change"], "body_changed");
    assert_eq!(cs[0]["cognitive"], 20);
    assert!(
        cs[0].get("callee_diff").is_none(),
        "the callee diff stays per file"
    );
    assert!(
        cs[1]["cognitive"].is_null(),
        "a deleted symbol has no metric"
    );

    for field in DROPPED {
        assert!(result.get(field).is_none(), "{field} was dropped: {result}");
    }
}

/// A co-change read that fails must say so, not render as "no partners"
/// (sutra/476). A malformed components.toml is the reachable failure.
#[test]
fn behavioral_coupling_failure_is_reported_not_empty() {
    let (dir, db) = setup_db_with_files();
    fs::create_dir_all(dir.path().join(".sutra")).unwrap();
    fs::write(dir.path().join(".sutra/components.toml"), "not = [valid").unwrap();
    let changed = vec!["src/core.rs".to_string()];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &changes(&db, &changed),
        &no_findings(),
    )
    .unwrap();

    let err = result["behavioral_coupling_error"]
        .as_str()
        .unwrap_or_default();
    assert!(
        err.contains("components.toml"),
        "expected the config error, got {result}"
    );
    assert!(result.get("behavioral_coupling").is_none());
}

/// Partners need repeated co-change: one shared commit is two files born or
/// swept together, not coupling (sutra/476).
#[test]
fn behavioral_coupling_requires_two_shared_commits() {
    let (dir, db) = setup_db_with_files();
    let core = db.file_by_path("src/core.rs").unwrap().unwrap().id;
    let helper = db.file_by_path("src/helper.rs").unwrap().unwrap().id;
    let lone = db
        .upsert_file("src/lone.rs", "rust", "h4", 10, true)
        .unwrap();
    let twin = db
        .upsert_file("src/twin.rs", "rust", "h5", 10, true)
        .unwrap();
    let commit = |hash: &str| sutra::db::CommitRow {
        hash: hash.into(),
        committed_at: 1,
        author: "x".into(),
        file_count: Some(2),
    };
    db.replace_commit_files(
        &[commit("c1"), commit("c2"), commit("c3")],
        &[
            ("c1".into(), core),
            ("c1".into(), helper),
            ("c2".into(), core),
            ("c2".into(), helper),
            ("c3".into(), lone),
            ("c3".into(), twin),
        ],
    )
    .unwrap();

    let changed = vec!["src/core.rs".to_string(), "src/lone.rs".to_string()];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &changes(&db, &changed),
        &no_findings(),
    )
    .unwrap();

    let partners: Vec<&str> = result["behavioral_coupling"]
        .as_array()
        .map(|a| a.iter().filter_map(|e| e["partner"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(partners, vec!["src/helper.rs"], "got {result}");
}

#[test]
fn unknown_files_handled_gracefully() {
    let (dir, db) = setup_db();
    let changed = vec!["src/nonexistent.rs".to_string()];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &changes(&db, &changed),
        &no_findings(),
    )
    .unwrap();

    let cf = result["changed_files"].as_array().unwrap();
    assert_eq!(cf.len(), 1);
    assert_eq!(cf[0]["path"], "src/nonexistent.rs");
    assert_eq!(cf[0]["symbol_changes"].as_array().unwrap().len(), 0);
}

// ── DD + FCA integration tests (v1/14) ──────────────────────────────

#[test]
fn constraint_violations_appear_in_output() {
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let findings = review::ReviewFindings {
        constraint_violations: vec![
            review::ConstraintFinding {
                constraint_id: "abc12345".into(),
                constraint_name: Some("no-core-internal".into()),
                constraint_kind: "forbidden_dep".into(),
                severity: Severity::Blocking,
                provenance: Some("docs/adr-001".into()),
                from_path: "src/core.rs".into(),
                to_path: "src/internal.rs".into(),
                component_context: None,
                detail: "forbidden: src/core.rs -> src/internal.rs".into(),
                delta: FindingDelta::Unknown,
                line: None,
                snippet: None,
                enclosing_symbol: None,
                justification: None,
                justify_marker: None,
            },
            review::ConstraintFinding {
                constraint_id: "builtin:cycles".into(),
                constraint_name: None,
                constraint_kind: "no_cycles".into(),
                // Un-owned builtin cycles are Advisory, not Blocking (sutra/359).
                severity: Severity::Advisory,
                provenance: None,
                from_path: "src/core.rs".into(),
                to_path: "src/helper.rs".into(),
                component_context: None,
                detail: "import cycle: src/core.rs -> src/helper.rs -> src/core.rs".into(),
                delta: FindingDelta::Unknown,
                line: None,
                snippet: None,
                enclosing_symbol: None,
                justification: None,
                justify_marker: None,
            },
        ],
        resolved_constraint_violations: vec![],
        waived_constraint_violations: vec![],
        constraint_violations_total: 2,
        ..Default::default()
    };

    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &changes(&db, &changed),
        &findings,
    )
    .unwrap();

    let cv = result["constraint_violations"].as_array().unwrap();
    assert_eq!(cv.len(), 2);
    assert_eq!(cv[0]["kind"], "forbidden_dep");
    assert_eq!(cv[0]["constraint_id"], "abc12345");
    assert_eq!(cv[0]["constraint_name"], "no-core-internal");
    assert_eq!(cv[0]["severity"], "blocking");
    assert_eq!(cv[0]["provenance"], "docs/adr-001");
    assert_eq!(cv[0]["from"], "src/core.rs");
    assert_eq!(cv[0]["to"], "src/internal.rs");
    assert_eq!(cv[1]["kind"], "no_cycles");
    assert_eq!(result["constraint_violations_total"].as_u64().unwrap(), 2);
}

/// A changed `.pyi` stub has no file row, so it never reaches `changed_ids`.
/// Review must still pattern-check it — this is the CI-facing enforcement path
/// for the exact files (public API stubs) the feature exists to govern.
#[test]
fn build_findings_checks_changed_pyi_stub() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "python"
query = '''
(function_definition
  name: (identifier) @_name (#eq? @_name "__new__")
  return_type: (type (identifier) @_ret) (#eq? @_ret "Never")) @match
'''
name = "no-new-returning-never"
severity = "blocking"
"#,
    )
    .unwrap();

    let pkg = dir.path().join("python/swisseph_rs");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        pkg.join("azalt.pyi"),
        "from typing import Never, final\n\n@final\nclass RefracDir:\n    \
         TRUE_TO_APP: RefracDir\n    def __new__(cls, _: Never, /) -> Never: ...\n",
    )
    .unwrap();

    // The stub is deliberately never indexed — no upsert_file for it.
    let changed = vec!["python/swisseph_rs/azalt.pyi".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        None,
        &registry,
    )
    .unwrap();

    let pattern: Vec<_> = findings
        .constraint_violations
        .iter()
        .filter(|v| v.constraint_kind == "forbidden_pattern")
        .collect();
    assert_eq!(
        pattern.len(),
        1,
        "changed .pyi should be pattern-checked: {:#?}",
        findings.constraint_violations
    );
    assert_eq!(pattern[0].from_path, "python/swisseph_rs/azalt.pyi");
}

/// The counterpart: an unchanged stub must not be dragged into a review.
#[test]
fn build_findings_ignores_unchanged_pyi_stub() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "python"
query = '''
(function_definition
  name: (identifier) @_name (#eq? @_name "__new__")
  return_type: (type (identifier) @_ret) (#eq? @_ret "Never")) @match
'''
name = "no-new-returning-never"
severity = "blocking"
"#,
    )
    .unwrap();

    let pkg = dir.path().join("python/swisseph_rs");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        pkg.join("azalt.pyi"),
        "from typing import Never, final\n\nclass RefracDir:\n    \
         def __new__(cls, _: Never, /) -> Never: ...\n",
    )
    .unwrap();
    fs::write(pkg.join("other.py"), "def f():\n    pass\n").unwrap();
    db.upsert_file("python/swisseph_rs/other.py", "python", "h1", 20, true)
        .unwrap();

    let changed = vec!["python/swisseph_rs/other.py".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        None,
        &registry,
    )
    .unwrap();

    assert!(
        findings
            .constraint_violations
            .iter()
            .all(|v| v.constraint_kind != "forbidden_pattern"),
        "unchanged stub must not be scanned: {:#?}",
        findings.constraint_violations
    );
}

// ── Integration test: build_findings exercises real DD + FCA path ────

#[test]
fn build_findings_integration_with_rules_and_imports() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    // Set up rules with a forbidden dep
    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[constraints]
forbidden_deps = [
  { from = "src/ui/*", to = "src/db/*" },
]
"#,
    )
    .unwrap();

    // Create files: src/ui/view.rs imports src/db/query.rs (forbidden)
    db.upsert_file("src/ui/view.rs", "rust", "h1", 100, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 80, true)
        .unwrap();

    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();

    // Symbols — pub functions without docs trigger FCA convention
    // when enough similar symbols establish the pattern
    db.insert_symbol(&sym(
        f_view.id,
        "view::render",
        "render",
        Some("fn render()"),
        1,
        20,
        Some(5),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        f_query.id,
        "query::fetch",
        "fetch",
        Some("fn fetch() -> Result<()>"),
        1,
        15,
        Some(3),
    ))
    .unwrap();

    // Create enough pub+has_doc functions to establish convention {kind:function, vis:pub} => {has_doc}
    for i in 0..6 {
        let path = format!("src/lib_{i}.rs");
        db.upsert_file(&path, "rust", &format!("lib{i}"), 50, true)
            .unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("lib_{i}::documented_fn");
        db.insert_symbol(&InsertSymbolParams {
            file_id: f.id,
            qualified_name: &qn,
            short_name: "documented_fn",
            kind: "function",
            signature: Some("fn documented_fn()"),
            signature_hash: None,
            structural_hash: None,
            visibility: Some("pub"),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: Some("A documented function"),
            cyclomatic: None,
            cognitive: Some(2),
            flags: 0,
            language_attrs: None,
        })
        .unwrap();
    }

    // Import edge: view.rs -> query.rs (triggers forbidden dep)
    db.insert_import(
        f_view.id,
        "src/db/query.rs",
        Some(f_query.id),
        1,
        "use",
        None,
    )
    .unwrap();

    let changed = vec!["src/ui/view.rs".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        None,
        &registry,
    )
    .unwrap();

    // DD should find the forbidden dep via maintained view
    assert!(
        !findings.constraint_violations.is_empty(),
        "should detect forbidden dep from src/ui/view.rs -> src/db/query.rs"
    );
    let cv = &findings.constraint_violations[0];
    assert_eq!(cv.constraint_kind, "forbidden_dep");
    assert!(!cv.constraint_id.is_empty());
    assert_eq!(cv.severity, Severity::Blocking);
    assert!(cv.detail.contains("src/ui/view.rs"));
    assert!(findings.constraint_violations_total >= 1);
}

#[test]
fn waived_constraint_violations_appear_in_output() {
    let (dir, db) = setup_db_with_files();
    let changed = vec!["src/core.rs".to_string()];

    let findings = review::ReviewFindings {
        constraint_violations: vec![],
        resolved_constraint_violations: vec![],
        waived_constraint_violations: vec![Waived {
            finding: review::ConstraintFinding {
                constraint_id: "abc12345".into(),
                constraint_name: Some("no-core-internal".into()),
                constraint_kind: "forbidden_dep".into(),
                severity: Severity::Blocking,
                provenance: None,
                from_path: "src/core.rs".into(),
                to_path: "src/internal.rs".into(),
                component_context: None,
                detail: "forbidden: src/core.rs -> src/internal.rs".into(),
                delta: FindingDelta::Unknown,
                line: None,
                snippet: None,
                enclosing_symbol: None,
                justification: None,
                justify_marker: None,
            },
            rationale: "legacy coupling".into(),
            waived_by: "josh".into(),
        }],
        constraint_violations_total: 1,
        ..Default::default()
    };

    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &changes(&db, &changed),
        &findings,
    )
    .unwrap();

    let cv = result["constraint_violations"].as_array().unwrap();
    assert!(
        cv.is_empty(),
        "waived constraint violations should not appear as regular violations"
    );

    let wcv = result["waived_constraint_violations"].as_array().unwrap();
    assert_eq!(wcv.len(), 1);
    assert_eq!(wcv[0]["constraint_id"], "abc12345");
    assert_eq!(wcv[0]["waived"], true);
    assert_eq!(wcv[0]["rationale"], "legacy coupling");
    assert_eq!(wcv[0]["waived_by"], "josh");
    assert_eq!(wcv[0]["kind"], "forbidden_dep");
}

#[test]
fn build_findings_constraint_delta_labels_introduced() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[constraints]
forbidden_deps = [
  { from = "src/ui/*", to = "src/db/*" },
]
"#,
    )
    .unwrap();

    db.upsert_file("src/ui/view.rs", "rust", "h1", 100, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 80, true)
        .unwrap();
    db.upsert_file("src/lib.rs", "rust", "h3", 50, true)
        .unwrap();

    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();
    let f_lib = db.file_by_path("src/lib.rs").unwrap().unwrap();

    db.insert_symbol(&sym(
        f_view.id,
        "view::render",
        "render",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        f_query.id,
        "query::fetch",
        "fetch",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(f_lib.id, "lib::init", "init", None, 1, 10, Some(2)))
        .unwrap();

    // view.rs imports query.rs (forbidden), lib.rs imports query.rs (not forbidden)
    db.insert_import(
        f_view.id,
        "src/db/query.rs",
        Some(f_query.id),
        1,
        "use",
        None,
    )
    .unwrap();
    db.insert_import(
        f_lib.id,
        "src/db/query.rs",
        Some(f_query.id),
        1,
        "use",
        None,
    )
    .unwrap();

    // Only view.rs is changed — its forbidden import should be detected as introduced
    let changed = vec!["src/ui/view.rs".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        None,
        &registry,
    )
    .unwrap();

    assert!(!findings.constraint_violations.is_empty());
    let v = &findings.constraint_violations[0];
    assert_eq!(v.constraint_kind, "forbidden_dep");
    assert!(v.detail.contains("[introduced]"));

    // Total should count ALL violations, not just those touching changed files
    assert!(findings.constraint_violations_total >= 1);
}

#[test]
fn build_findings_partitions_constraint_waivers() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    // Named constraint: with waivers now file-authoritative and keyed by
    // constraint NAME (sutra/303/308), a portable waiver needs a name to
    // resolve — an id-keyed fallback cannot (resolution is name-only).
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/ui/*"
to = "src/db/*"
name = "ui-not-db"
"#,
    )
    .unwrap();

    db.upsert_file("src/ui/view.rs", "rust", "h1", 100, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 80, true)
        .unwrap();

    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();

    db.insert_symbol(&sym(
        f_view.id,
        "view::render",
        "render",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        f_query.id,
        "query::fetch",
        "fetch",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();

    db.insert_import(
        f_view.id,
        "src/db/query.rs",
        Some(f_query.id),
        1,
        "use",
        None,
    )
    .unwrap();

    // Seed a legacy DB waiver (constraint_name carried) and let the DD review
    // path migrate it into .sutra/accepted.toml, then honor it — the sutra/308
    // hazard-1 migration: waivers that lived only in the DB must survive the
    // move to file-authoritative, not be reprojected away.
    let (constraints, _parse_errors) = sutra::rules::load_rules(dir.path())
        .unwrap()
        .all_constraints();
    let constraint_id = &constraints[0].id;
    db.create_constraint_waiver(
        constraint_id,
        Some("ui-not-db"),
        "src/ui/view.rs",
        None,
        "legacy coupling, will be removed in next sprint",
        "josh",
    )
    .unwrap();

    let changed = vec!["src/ui/view.rs".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        None,
        &registry,
    )
    .unwrap();

    assert!(
        findings.constraint_violations.is_empty(),
        "waived violations should not appear in constraint_violations"
    );
    assert_eq!(findings.waived_constraint_violations.len(), 1);
    let w = &findings.waived_constraint_violations[0];
    assert_eq!(w.finding.constraint_id, *constraint_id);
    assert_eq!(
        w.rationale,
        "legacy coupling, will be removed in next sprint"
    );
    assert_eq!(w.waived_by, "josh");
    assert_eq!(w.finding.from_path, "src/ui/view.rs");
    assert_eq!(
        findings.constraint_violations_total, 1,
        "total should include waived violations"
    );
}

#[test]
fn convention_pipeline_persists_conventions_to_db() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    // 40 pub functions: 38 with signatures (95%), enough for FCA to find
    // the implication {kind:function, vis:pub} => {has_sig} at 0.95 confidence.
    for i in 0..40 {
        let path = format!("src/f_{i}.rs");
        db.upsert_file(&path, "rust", &format!("f{i}"), 50, true)
            .unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("f_{i}::process");
        let sig = if i < 38 { Some("fn process()") } else { None };
        let doc = if i % 5 == 0 { Some("docs") } else { None };
        db.insert_symbol(&InsertSymbolParams {
            file_id: f.id,
            qualified_name: &qn,
            short_name: "process",
            kind: "function",
            signature: sig,
            signature_hash: None,
            structural_hash: None,
            visibility: Some("pub"),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: doc,
            cyclomatic: None,
            cognitive: Some(2),
            flags: 0,
            language_attrs: None,
        })
        .unwrap();
    }

    assert!(db.all_conventions().unwrap().is_empty());

    let registry = default_registry();
    let outcome = sutra::conventions::pipeline::rebuild(&db, &registry, dir.path()).unwrap();
    assert!(outcome.convention_count > 0);

    let conventions = db.all_conventions().unwrap();
    assert!(
        !conventions.is_empty(),
        "conventions should be persisted to DB after pipeline::rebuild"
    );
    for c in &conventions {
        assert!(!c.id.is_empty());
        assert!(!c.antecedent.is_empty());
        assert!(!c.consequent.is_empty());
        assert!(!c.first_seen.is_empty());
        assert!(!c.last_seen.is_empty());
    }
}

#[test]
fn build_findings_resyncs_shared_engine_holding_a_stale_graph() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[constraints]
forbidden_deps = [
    { from = "src/ui/**", to = "src/db/**" }
]
"#,
    )
    .unwrap();

    // Two files with a forbidden dep
    db.upsert_file("src/ui/view.rs", "rust", "h1", 10, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 10, true)
        .unwrap();
    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();
    db.insert_import(
        f_view.id,
        "src/db/query.rs",
        Some(f_query.id),
        1,
        "use",
        None,
    )
    .unwrap();

    // Seed the shared engine with a graph that no longer matches the index —
    // the shape a reparse leaves behind, since file ids are reminted.
    let shared = DdEngine::new(Duration::from_secs(600));
    shared
        .ingest(sutra::constraints::DdFacts {
            import_edges: vec![(f_view.id + 900, f_query.id + 900)],
        })
        .unwrap();

    // build_findings must resync the engine and still detect the violation
    let changed = vec!["src/ui/view.rs".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        Some(&shared),
        &registry,
    )
    .unwrap();
    assert!(
        !findings.constraint_violations.is_empty(),
        "a shared engine holding a stale graph should be resynced, not trusted"
    );
}

#[test]
fn changed_files_include_freshness() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    // Create actual file on disk BEFORE DB upsert so last_parsed > mtime → fresh
    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("fresh.rs"), "fn fresh() {}").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));

    db.upsert_file("src/fresh.rs", "rust", "h1", 10, true)
        .unwrap();
    let f = db.file_by_path("src/fresh.rs").unwrap().unwrap();
    db.insert_symbol(&sym(
        f.id,
        "fresh::fresh",
        "fresh",
        Some("fn fresh()"),
        1,
        5,
        Some(2),
    ))
    .unwrap();
    db.update_rollups(f.id, 0, 1).unwrap();

    let changed = vec!["src/fresh.rs".to_string()];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &changes(&db, &changed),
        &no_findings(),
    )
    .unwrap();

    let cf = result["changed_files"].as_array().unwrap();
    assert_eq!(cf[0]["_freshness"], "fresh");
}

#[test]
fn freshness_reflects_actual_file_state() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let src = dir.path().join("src");
    fs::create_dir_all(&src).unwrap();

    // File created before DB insert → fresh
    fs::write(src.join("fresh.rs"), "fn a() {}").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    db.upsert_file("src/fresh.rs", "rust", "h1", 10, true)
        .unwrap();

    // File modified after DB insert → edited_uncommitted
    db.upsert_file("src/edited.rs", "rust", "h2", 10, true)
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    fs::write(src.join("edited.rs"), "fn b() { changed }").unwrap();

    // File not on disk → stale_index
    db.upsert_file("src/gone.rs", "rust", "h3", 10, true)
        .unwrap();

    for path in &["src/fresh.rs", "src/edited.rs", "src/gone.rs"] {
        let f = db.file_by_path(path).unwrap().unwrap();
        db.insert_symbol(&sym(f.id, &format!("{path}::f"), "f", None, 1, 5, Some(1)))
            .unwrap();
        db.update_rollups(f.id, 0, 1).unwrap();
    }

    let changed = vec![
        "src/fresh.rs".to_string(),
        "src/edited.rs".to_string(),
        "src/gone.rs".to_string(),
    ];
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &changes(&db, &changed),
        &no_findings(),
    )
    .unwrap();

    let cf = result["changed_files"].as_array().unwrap();
    let freshness: std::collections::HashMap<&str, &str> = cf
        .iter()
        .map(|f| {
            (
                f["path"].as_str().unwrap(),
                f["_freshness"].as_str().unwrap(),
            )
        })
        .collect();

    assert_eq!(freshness["src/fresh.rs"], "fresh");
    assert_eq!(freshness["src/edited.rs"], "edited_uncommitted");
    assert_eq!(freshness["src/gone.rs"], "stale_index");
}

#[test]
fn build_findings_surfaces_error_on_bad_rules() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(rules_dir.join("rules.toml"), "{{invalid toml").unwrap();

    let registry = default_registry();
    let result = review::build_findings(
        &db,
        dir.path(),
        &["src/foo.rs".to_string()],
        "HEAD",
        whole_files(&["src/foo.rs".to_string()]).head(),
        None,
        &registry,
    );
    assert!(
        result.is_err(),
        "malformed rules.toml should return Err, not empty findings"
    );
}

#[test]
fn build_findings_cycle_violations_counted_in_total() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(rules_dir.join("rules.toml"), "[constraints]\n").unwrap();

    db.upsert_file("src/a.rs", "rust", "ha", 50, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 50, true).unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();

    // a -> b -> a (cycle)
    db.insert_import(fa.id, "src/b.rs", Some(fb.id), 1, "use", None)
        .unwrap();
    db.insert_import(fb.id, "src/a.rs", Some(fa.id), 1, "use", None)
        .unwrap();

    let changed = vec!["src/a.rs".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        None,
        &registry,
    )
    .unwrap();

    assert!(
        findings
            .constraint_violations
            .iter()
            .any(|v| v.constraint_kind == "no_cycles"),
        "should detect the import cycle"
    );
    assert_eq!(
        findings.constraint_violations_total,
        findings.constraint_violations.len(),
        "total should equal the number of violations (including cycles)"
    );
}

#[test]
fn resolved_delta_for_removed_forbidden_edge() {
    use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
    use sutra::parser::adapter::default_registry;

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[constraints]
forbidden_deps = [
  { from = "src/ui/*", to = "src/db/*" },
]
"#,
    )
    .unwrap();

    db.upsert_file("src/ui/view.rs", "rust", "h1", 100, true)
        .unwrap();
    db.upsert_file("src/db/query.rs", "rust", "h2", 80, true)
        .unwrap();
    db.upsert_file("src/lib.rs", "rust", "h3", 50, true)
        .unwrap();

    let f_view = db.file_by_path("src/ui/view.rs").unwrap().unwrap();
    let f_query = db.file_by_path("src/db/query.rs").unwrap().unwrap();
    let f_lib = db.file_by_path("src/lib.rs").unwrap().unwrap();

    db.insert_symbol(&sym(
        f_view.id,
        "view::render",
        "render",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        f_query.id,
        "query::fetch",
        "fetch",
        None,
        1,
        10,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(f_lib.id, "lib::init", "init", None, 1, 10, Some(2)))
        .unwrap();

    // Current DB: only lib.rs -> query.rs (allowed). The forbidden view.rs -> query.rs was removed.
    db.insert_import(
        f_lib.id,
        "src/db/query.rs",
        Some(f_query.id),
        1,
        "use",
        None,
    )
    .unwrap();

    // old_edges records that view.rs previously imported query.rs (forbidden)
    let changed_ids: std::collections::HashSet<i64> = [f_view.id].into_iter().collect();
    let old_edges: std::collections::HashSet<(i64, i64)> =
        [(f_view.id, f_query.id)].into_iter().collect();
    let changed_paths: std::collections::HashSet<&str> = std::collections::HashSet::new();

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::ChangedFiles {
            changed_ids: &changed_ids,
            old_edges: &old_edges,
            import_delta: &Default::default(),
            changed_pattern_only_paths: &[],
            content: sutra::constraints::check::ContentSource::Worktree,
            changed_paths: &changed_paths,
            added_lines: &Default::default(),
        },
        &registry,
    )
    .unwrap();

    assert!(
        outcome.active.is_empty(),
        "no current violation since forbidden edge was removed"
    );
    assert_eq!(
        outcome.resolved.len(),
        1,
        "removed forbidden edge should appear as resolved"
    );
    assert_eq!(outcome.resolved[0].delta, FindingDelta::Resolved);
    assert!(outcome.resolved[0].from_path.contains("ui/view.rs"));
    assert!(outcome.resolved[0].to_path.contains("db/query.rs"));
}

#[test]
fn build_findings_includes_pattern_violations() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(unsafe_block) @match'
name = "no-unsafe"
severity = "advisory"
scope = "src/"
"#,
    )
    .unwrap();

    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("core.rs"),
        "fn process() { unsafe { std::ptr::null::<u8>().read() }; }\n",
    )
    .unwrap();
    fs::write(src_dir.join("safe.rs"), "fn safe() { let x = 1; }\n").unwrap();

    db.upsert_file("src/core.rs", "rust", "h1", 1, true)
        .unwrap();
    db.upsert_file("src/safe.rs", "rust", "h2", 1, true)
        .unwrap();
    db.insert_symbol(&sym(
        db.file_by_path("src/core.rs").unwrap().unwrap().id,
        "process",
        "process",
        None,
        1,
        1,
        Some(1),
    ))
    .unwrap();

    let changed = vec!["src/core.rs".to_string(), "src/safe.rs".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        None,
        &registry,
    )
    .unwrap();

    let pattern_violations: Vec<_> = findings
        .constraint_violations
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_pattern")
        .collect();
    assert_eq!(
        pattern_violations.len(),
        1,
        "unsafe block in src/core.rs should trigger one finding"
    );
    assert_eq!(pattern_violations[0].from_path, "src/core.rs");
    assert!(pattern_violations[0].line.is_some());
    assert!(pattern_violations[0].snippet.is_some());

    let safe_violations: Vec<_> = findings
        .constraint_violations
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_pattern" && f.from_path == "src/safe.rs")
        .collect();
    assert!(
        safe_violations.is_empty(),
        "safe.rs has no unsafe blocks, should have no pattern findings"
    );
}

#[test]
fn build_findings_pattern_scope_filters_files() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(unsafe_block) @match'
name = "no-unsafe-in-src"
scope = "src/"
"#,
    )
    .unwrap();

    let src_dir = dir.path().join("src");
    let tests_dir = dir.path().join("tests");
    fs::create_dir_all(&src_dir).unwrap();
    fs::create_dir_all(&tests_dir).unwrap();
    fs::write(src_dir.join("lib.rs"), "fn f() { unsafe { }; }\n").unwrap();
    fs::write(tests_dir.join("test.rs"), "fn t() { unsafe { }; }\n").unwrap();

    db.upsert_file("src/lib.rs", "rust", "h1", 1, true).unwrap();
    db.upsert_file("tests/test.rs", "rust", "h2", 1, true)
        .unwrap();

    let changed = vec!["src/lib.rs".to_string(), "tests/test.rs".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        None,
        &registry,
    )
    .unwrap();

    let pattern_violations: Vec<_> = findings
        .constraint_violations
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_pattern")
        .collect();
    assert_eq!(
        pattern_violations.len(),
        1,
        "only src/lib.rs is in scope, tests/test.rs should be excluded"
    );
    assert_eq!(pattern_violations[0].from_path, "src/lib.rs");
}

#[test]
fn build_findings_pattern_waiver_suppresses_finding() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(unsafe_block) @match'
name = "no-unsafe"
"#,
    )
    .unwrap();

    let src_dir = dir.path().join("src");
    fs::create_dir_all(&src_dir).unwrap();
    fs::write(
        src_dir.join("ffi.rs"),
        "fn bridge() { unsafe { libc::exit(0) }; }\n",
    )
    .unwrap();

    db.upsert_file("src/ffi.rs", "rust", "h1", 1, true).unwrap();

    // Load rules to get the constraint ID
    let mut rules = sutra::rules::load_rules(dir.path()).unwrap();
    let (constraints, _) = rules.all_constraints();
    let pattern_constraint = constraints
        .iter()
        .find(|c| {
            matches!(
                c.kind,
                sutra::rules::ConstraintKind::ForbiddenPattern { .. }
            )
        })
        .unwrap();

    // File-level waiver: suppresses all findings in src/ffi.rs
    db.create_constraint_waiver(
        &pattern_constraint.id,
        pattern_constraint.name.as_deref(),
        "src/ffi.rs",
        None,
        "FFI boundary, unsafe is required",
        "josh",
    )
    .unwrap();

    let changed = vec!["src/ffi.rs".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        None,
        &registry,
    )
    .unwrap();

    let active_pattern: Vec<_> = findings
        .constraint_violations
        .iter()
        .filter(|f| f.constraint_kind == "forbidden_pattern")
        .collect();
    assert!(
        active_pattern.is_empty(),
        "file-level waiver should suppress pattern finding"
    );

    let waived_pattern: Vec<_> = findings
        .waived_constraint_violations
        .iter()
        .filter(|w| w.finding.constraint_kind == "forbidden_pattern")
        .collect();
    assert_eq!(
        waived_pattern.len(),
        1,
        "waived pattern finding should appear in waived list"
    );
}

/// Report-only instance acks (sutra/305) must stay visible on the review surface,
/// not silently drop out of the count (sutra/306). An acked clone is subtracted
/// from constraint_violations but surfaced in the `acknowledged` array — parity
/// with how waivers appear in waived_constraint_violations.
#[test]
fn instance_acks_surface_on_review() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    fs::create_dir_all(&rules_dir).unwrap();
    fs::write(
        rules_dir.join("rules.toml"),
        r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(call_expression function: (field_expression field: (field_identifier) @m (#eq? @m "clone"))) @match'
name = "no-clone"
severity = "blocking"
scope = "src/"
"#,
    )
    .unwrap();

    // Two byte-identical foo.clone() in one fn, plus one distinct bar.clone().
    let src = "fn a() {\n    let x = foo.clone();\n    let y = foo.clone();\n    \
               let w = bar.clone();\n}\n";
    let src_path = dir.path().join("src/lib.rs");
    fs::create_dir_all(src_path.parent().unwrap()).unwrap();
    fs::write(&src_path, src).unwrap();
    db.upsert_file("src/lib.rs", "rust", "h1", 5, true).unwrap();

    let rule_id = {
        let mut loaded = sutra::rules::load_rules(dir.path()).unwrap();
        let (cs, _) = loaded.all_constraints();
        cs.iter()
            .find(|c| c.name.as_deref() == Some("no-clone"))
            .unwrap()
            .id
            .to_string()
    };

    // Ack both foo.clone() instances; bar.clone() is a different key, untouched.
    db.create_constraint_instance_ack(
        &rule_id,
        Some("no-clone"),
        "src/lib.rs",
        Some("a"),
        Some("foo.clone()"),
        2,
        Some("owned-required"),
        "josh",
    )
    .unwrap();

    let changed = vec!["src/lib.rs".to_string()];
    let registry = default_registry();
    let findings = review::build_findings(
        &db,
        dir.path(),
        &changed,
        "HEAD",
        whole_files(&changed).head(),
        None,
        &registry,
    )
    .unwrap();

    // The two acked foo clones are gone; only the distinct bar clone remains.
    let pattern: Vec<_> = findings
        .constraint_violations
        .iter()
        .filter(|v| v.constraint_kind == "forbidden_pattern")
        .collect();
    assert_eq!(
        pattern.len(),
        1,
        "both foo clones acked -> only bar reported: {:#?}",
        findings.constraint_violations
    );
    assert_eq!(pattern[0].snippet.as_deref(), Some("bar.clone()"));

    // The acked state is surfaced on findings, not silent.
    assert_eq!(findings.acknowledged.len(), 1, "the acked key is surfaced");
    assert_eq!(findings.acknowledged[0]["snippet"], "foo.clone()");
    assert_eq!(findings.acknowledged[0]["accepted_count"], 2);

    // ...and it makes it into the serialized review output.
    let result = review::compute(
        &db,
        dir.path(),
        &changed,
        &changes(&db, &changed),
        &findings,
    )
    .unwrap();
    let acked = result["acknowledged"].as_array().unwrap();
    assert_eq!(acked.len(), 1);
    assert_eq!(acked[0]["snippet"], "foo.clone()");
    assert_eq!(acked[0]["rationale"], "owned-required");
}

/// Two-commit repo for diff-spec resolution tests.
fn two_commit_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(args)
            .output()
            .expect("git spawn");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    fs::write(dir.path().join("a.rs"), "fn a() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "--no-verify", "-m", "one"]);
    fs::write(dir.path().join("a.rs"), "fn a() { let _ = 1; }\n").unwrap();
    git(&["commit", "-q", "--no-verify", "-am", "two"]);
    dir
}

fn rev_parse(root: &std::path::Path, rev: &str) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", rev])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn diff_spec_resolves_both_sides_to_oids() {
    let repo = two_commit_repo();
    let root = repo.path();
    let (head, parent) = (rev_parse(root, "HEAD"), rev_parse(root, "HEAD~1"));
    for spec in ["HEAD~1..HEAD", "HEAD", "HEAD~1.."] {
        let scope = review::resolve_diff_entries(root, spec).unwrap();
        assert_eq!(scope.base_revision, parent, "{spec}");
        assert_eq!(
            scope.head_revision.as_deref(),
            Some(head.as_str()),
            "{spec}"
        );
        assert_eq!(scope.paths(), vec!["a.rs".to_string()], "{spec}");
    }
    // Staged/unstaged sentinels are untouched.
    let staged = review::resolve_diff_entries(root, "staged").unwrap();
    assert_eq!(
        (
            staged.base_revision.as_str(),
            staged.head_revision.as_deref()
        ),
        ("HEAD", Some(""))
    );
    let unstaged = review::resolve_diff_entries(root, "unstaged").unwrap();
    assert_eq!(
        (unstaged.base_revision.as_str(), unstaged.head_revision),
        ("", None)
    );
}

#[test]
fn option_like_diff_spec_is_rejected_and_writes_nothing() {
    let repo = two_commit_repo();
    let root = repo.path();
    let out = tempfile::tempdir().unwrap();
    let target = out.path().join("clobbered");
    let opt = format!("--output={}", target.display());
    for spec in [format!("{opt}..HEAD"), format!("HEAD..{opt}"), opt.clone()] {
        let err = review::resolve_diff_entries(root, &spec);
        assert!(err.is_err(), "spec {spec:?} must be rejected");
    }
    // The shared git sinks other tools call directly must not read it as an
    // option either.
    assert!(sutra::git::git_diff_files(root, &opt, "HEAD").is_err());
    let _ = sutra::git::git_file_content_at(root, &opt, "a.rs");
    let _ = sutra::git::git_diff_hunks(root, &opt, Some("HEAD"));
    assert!(
        !target.exists(),
        "an option-like revision reached git as an option"
    );
}

/// sutra/517 end to end: a commit that edits one method of the second of two
/// `impl Foo` blocks reports that method, and neither block, on both the
/// commit range and the worktree side.
#[test]
fn changed_symbols_scope_to_the_edited_method() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .expect("git spawn");
        assert!(out.status.success(), "git {args:?}");
    };
    let before = "struct Foo;\n\nimpl Foo {\n    fn a(&self) {\n        one();\n    }\n}\n\n\
        impl Foo {\n    fn b(&self) {\n        two();\n    }\n}\n";
    git(&["init", "-q", "-b", "main"]);
    git(&["config", "user.email", "t@example.com"]);
    git(&["config", "user.name", "t"]);
    fs::write(root.join("a.rs"), before).unwrap();
    git(&["add", "."]);
    git(&["commit", "-q", "--no-verify", "-m", "one"]);
    fs::write(root.join("a.rs"), before.replace("two()", "three()")).unwrap();

    let (_db_dir, db) = setup_db();
    let summary = |changed: &ChangedSymbols| -> Vec<(String, String)> {
        changed
            .iter()
            .map(|(_, c, _)| (c.symbol.to_string(), c.change.as_str().to_string()))
            .collect()
    };
    let expected = vec![("Foo::b".to_string(), "body_changed".to_string())];

    let worktree = review::resolve_diff_entries(root, "unstaged").unwrap();
    assert_eq!(
        summary(&review::changed_symbols(&db, root, &worktree).unwrap()),
        expected
    );

    git(&["commit", "-q", "--no-verify", "-am", "two"]);
    let range = review::resolve_diff_entries(root, "HEAD~1..HEAD").unwrap();
    let changed = review::changed_symbols(&db, root, &range).unwrap();
    assert_eq!(summary(&changed), expected);
    let cd = changed.files[0].changes[0].callee_diff.as_ref().unwrap();
    assert_eq!(
        (cd.added.as_slice(), cd.removed.as_slice()),
        (&["three".to_string()][..], &["two".to_string()][..])
    );
}
