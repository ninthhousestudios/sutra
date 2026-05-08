use sutra::tools::provenance;

#[test]
fn classify_conventional_commits() {
    assert_eq!(provenance::classify("feat: add login"), "feature");
    assert_eq!(provenance::classify("feat(auth): add login"), "feature");
    assert_eq!(provenance::classify("fix: null pointer crash"), "bugfix");
    assert_eq!(provenance::classify("fix(db): connection leak"), "bugfix");
    assert_eq!(provenance::classify("refactor: extract helper"), "refactor");
    assert_eq!(provenance::classify("test: add coverage"), "test");
    assert_eq!(provenance::classify("docs: update readme"), "docs");
    assert_eq!(provenance::classify("doc: api reference"), "docs");
    assert_eq!(provenance::classify("chore: bump deps"), "chore");
    assert_eq!(provenance::classify("perf: cache lookups"), "performance");
    assert_eq!(provenance::classify("ci: add workflow"), "chore");
    assert_eq!(provenance::classify("build: update makefile"), "chore");
    assert_eq!(provenance::classify("style: format code"), "chore");
}

#[test]
fn classify_unknown_fallback() {
    assert_eq!(provenance::classify("update stuff"), "unknown");
    assert_eq!(provenance::classify("WIP"), "unknown");
    assert_eq!(provenance::classify(""), "unknown");
}

#[test]
fn compute_returns_chronological_commits() {
    let commits = vec![
        provenance::CommitInfo {
            sha: "abc123".into(),
            author: "alice".into(),
            date: "2026-01-15T10:00:00+00:00".into(),
            message: "feat: add widget".into(),
        },
        provenance::CommitInfo {
            sha: "def456".into(),
            author: "bob".into(),
            date: "2026-02-20T14:30:00+00:00".into(),
            message: "fix: widget crash".into(),
        },
    ];

    let result = provenance::compute("mod::widget", "src/widget.rs", &commits);

    assert_eq!(result["symbol"], "mod::widget");
    assert_eq!(result["file"], "src/widget.rs");

    let entries = result["commits"].as_array().unwrap();
    assert_eq!(entries.len(), 2);

    assert_eq!(entries[0]["sha"], "abc123");
    assert_eq!(entries[0]["classification"], "feature");
    assert_eq!(entries[1]["sha"], "def456");
    assert_eq!(entries[1]["classification"], "bugfix");
}

#[test]
fn compute_empty_commits() {
    let result = provenance::compute("mod::gone", "src/gone.rs", &[]);
    assert_eq!(result["commits"].as_array().unwrap().len(), 0);
    assert_eq!(result["total"], 0);
}
