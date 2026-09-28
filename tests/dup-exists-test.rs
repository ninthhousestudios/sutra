//! The dup-exists advisory end to end (sutra/469): a commit adds code like a
//! function that already exists, and the review names the existing one.

mod support;

use support::{Fixture, git, index, repo, write};
use sutra::parser::adapter::default_registry;
use sutra::tools::dup_exists::{self, Advisory, UnitKind};
use sutra::tools::review;

const LIB: &str = "pub mod a;\npub mod b;\npub mod c;\n";

const LOAD: &str = "pub fn load_rows(conn: &Conn) -> Vec<Row> {
    let mut stmt = conn.prepare(\"SELECT id, name, score FROM rows WHERE active = 1\");
    let rows = stmt.query_map(|r| Row { id: r.get(0), name: r.get(1), score: r.get(2) });
    let kept: Vec<Row> = rows.into_iter().filter(|r| r.score > 10).collect();
    kept
}
";

/// Unrelated functions, so the corpus has more than the pair under test.
const OTHER: &str = "pub fn render(title: &str, width: usize) -> String {
    let mut out = String::with_capacity(width);
    out.push_str(title);
    while out.len() < width { out.push('-'); }
    out
}

pub fn parse_flags(args: &[String]) -> (bool, bool) {
    let verbose = args.iter().any(|a| a == \"-v\");
    let quiet = args.iter().any(|a| a == \"-q\");
    let both = verbose && quiet;
    (verbose && !both, quiet)
}
";

fn copy_named(name: &str) -> String {
    LOAD.replace("load_rows", name)
}

fn fixture(commits: &[&[(&str, &str)]]) -> Fixture {
    index(repo(commits), "dup-exists")
}

fn review_diff(fx: &Fixture, diff: &str) -> Advisory {
    let scope = review::resolve_diff_entries(fx.root.path(), diff).unwrap();
    let registry = default_registry();
    let sibling = sutra::tools::sibling_pattern::run_advisory(
        &fx.db,
        fx.root.path(),
        &scope,
        &registry,
        "check",
        diff,
    );
    dup_exists::run_advisory(
        &fx.db,
        fx.root.path(),
        &scope,
        &registry,
        ("check", diff),
        sibling.patch(),
    )
}

/// `(kind, unit, match)` for every pair.
fn pairs(advisory: &Advisory) -> Vec<(UnitKind, &str, &str)> {
    assert!(advisory.error.is_none(), "{:?}", advisory.error);
    assert!(advisory.skipped.is_none(), "{:?}", advisory.skipped);
    assert!(
        advisory.report.incomplete.is_empty(),
        "{:?}",
        advisory.report.incomplete
    );
    let report = &advisory.report;
    report
        .findings
        .iter()
        .flat_map(|f| {
            f.matches.iter().map(move |m| {
                (
                    f.kind,
                    report.unit(f).qualified_name.as_str(),
                    report.matched(m).qualified_name.as_str(),
                )
            })
        })
        .collect()
}

#[test]
fn an_added_copy_names_the_function_that_exists() {
    let fx = fixture(&[
        &[("src/lib.rs", LIB), ("src/a.rs", LOAD), ("src/c.rs", OTHER)],
        &[("src/b.rs", &copy_named("fetch_rows"))],
    ]);
    let advisory = review_diff(&fx, "HEAD");
    assert_eq!(
        pairs(&advisory),
        vec![(UnitKind::Added, "fetch_rows", "load_rows")]
    );
    let out = advisory.to_json();
    let group = &out["groups"][0];
    assert_eq!(group["file"], "src/b.rs");
    assert_eq!(group["matched_file"], "src/a.rs");
    let pair = &group["pairs"][0];
    assert_eq!(pair["match_line"], 1);
    assert!(pair["shared_runs"].as_u64().unwrap() >= 6, "{pair}");
    assert!(
        pair["shared"].as_str().unwrap().contains("conn.prepare("),
        "{pair}"
    );
    assert_eq!(
        fx.db
            .firings(Some(dup_exists::MECHANISM), None)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_function_that_calls_the_match_is_reuse() {
    let wrapper = copy_named("fetch_rows")
        .replace("    kept\n", "    crate::a::load_rows(conn);\n    kept\n");
    let fx = fixture(&[
        &[("src/lib.rs", LIB), ("src/a.rs", LOAD), ("src/c.rs", OTHER)],
        &[("src/b.rs", &wrapper)],
    ]);
    assert_eq!(pairs(&review_diff(&fx, "HEAD")), vec![]);
}

/// A reference that is not a call (a value read of the function) is not
/// reuse: the copy still duplicates the match (sutra/505).
#[test]
fn a_value_reference_to_the_match_is_not_reuse() {
    let wrapper = copy_named("fetch_rows").replace(
        "    kept\n",
        "    let _loader = crate::a::load_rows;\n    kept\n",
    );
    let fx = fixture(&[
        &[("src/lib.rs", LIB), ("src/a.rs", LOAD), ("src/c.rs", OTHER)],
        &[("src/b.rs", &wrapper)],
    ]);
    assert_eq!(
        pairs(&review_diff(&fx, "HEAD")),
        vec![(UnitKind::Added, "fetch_rows", "load_rows")]
    );
}

/// The extraction commit itself: the new helper matches the code it came
/// from, which now calls it (mid-extraction, before the old block is gone).
#[test]
fn the_function_a_helper_is_extracted_from_is_not_a_match() {
    let inline = LOAD.replace(
        "pub fn load_rows(conn: &Conn) -> Vec<Row> {",
        "pub fn report(conn: &Conn) -> usize {\n    let banner = 1;",
    );
    let calling = inline.replace("    kept\n", "    let _ = load_rows(conn);\n    kept\n");
    let fx = fixture(&[
        &[
            ("src/lib.rs", LIB),
            ("src/a.rs", &inline),
            ("src/c.rs", OTHER),
        ],
        &[("src/a.rs", &format!("{calling}\n{LOAD}"))],
    ]);
    assert_eq!(pairs(&review_diff(&fx, "HEAD")), vec![]);
}

/// The code a helper came from only naming it as a value has not had the
/// block extracted: the pair still fires (sutra/505).
#[test]
fn a_value_reference_from_the_old_code_is_not_extraction() {
    let inline = LOAD.replace(
        "pub fn load_rows(conn: &Conn) -> Vec<Row> {",
        "pub fn report(conn: &Conn) -> usize {\n    let banner = 1;",
    );
    let naming = inline.replace("    kept\n", "    let _loader = load_rows;\n    kept\n");
    let fx = fixture(&[
        &[
            ("src/lib.rs", LIB),
            ("src/a.rs", &inline),
            ("src/c.rs", OTHER),
        ],
        &[("src/a.rs", &format!("{naming}\n{LOAD}"))],
    ]);
    assert_eq!(
        pairs(&review_diff(&fx, "HEAD")),
        vec![(UnitKind::Added, "load_rows", "report")]
    );
}

#[test]
fn two_copies_added_together_are_one_pair() {
    let fx = fixture(&[
        &[("src/lib.rs", LIB), ("src/c.rs", OTHER)],
        &[("src/a.rs", LOAD), ("src/b.rs", &copy_named("fetch_rows"))],
    ]);
    assert_eq!(
        pairs(&review_diff(&fx, "HEAD")),
        vec![(UnitKind::Added, "load_rows", "fetch_rows")]
    );
}

/// A staged deletion recreated in the worktree: the index holds the file, but
/// the reviewed side does not, so its functions are not matches (sutra/506).
#[test]
fn a_function_the_reviewed_side_deleted_is_not_a_match() {
    let root = repo(&[&[("src/lib.rs", LIB), ("src/a.rs", LOAD), ("src/c.rs", OTHER)]]);
    let r = root.path();
    git(r, &["rm", "-q", "src/a.rs"]);
    write(r, "src/a.rs", LOAD);
    // Enough unrelated code that git does not pair a.rs and b.rs as a rename.
    let filler: String = (0..12)
        .map(|i| format!("pub fn step_{i}(x: u32) -> u32 {{ x.wrapping_mul({i}) }}\n"))
        .collect();
    // An edited copy, so it is not taken for load_rows moved.
    let copy =
        copy_named("fetch_rows").replace("    kept\n", "    let _n = kept.len();\n    kept\n");
    write(r, "src/b.rs", &format!("{copy}{filler}"));
    git(r, &["add", "src/b.rs"]);
    let fx = index(root, "dup-exists");
    let scope = review::resolve_diff_entries(fx.root.path(), "staged").unwrap();
    assert!(
        scope.entries.iter().all(|e| e.old_path.is_none()),
        "{:?}",
        scope.entries
    );
    assert_eq!(pairs(&review_diff(&fx, "staged")), vec![]);
}

#[test]
fn a_block_copied_into_an_existing_function_fires_as_modified() {
    let before = "pub fn summarize(conn: &Conn) -> usize {\n    let n = 0;\n    n\n}\n";
    let after = "pub fn summarize(conn: &Conn) -> usize {
    let n = 0;
    let mut stmt = conn.prepare(\"SELECT id, name, score FROM rows WHERE active = 1\");
    let rows = stmt.query_map(|r| Row { id: r.get(0), name: r.get(1), score: r.get(2) });
    let kept: Vec<Row> = rows.into_iter().filter(|r| r.score > 10).collect();
    let total = kept.len();
    n + total
}
";
    let fx = fixture(&[
        &[
            ("src/lib.rs", LIB),
            ("src/a.rs", LOAD),
            ("src/b.rs", before),
            ("src/c.rs", OTHER),
        ],
        &[("src/b.rs", after)],
    ]);
    let advisory = review_diff(&fx, "HEAD");
    assert_eq!(
        pairs(&advisory),
        vec![(UnitKind::Modified, "summarize", "load_rows")]
    );
    // The site is the first added line, not the declaration.
    assert_eq!(advisory.report.findings[0].line, 3);
}

#[test]
fn test_code_is_neither_a_unit_nor_a_match() {
    let tests = format!(
        "#[cfg(test)]\nmod tests {{\n{}}}\n",
        copy_named("fetch_rows")
    );
    let fx = fixture(&[
        &[("src/lib.rs", LIB), ("src/a.rs", LOAD), ("src/c.rs", OTHER)],
        &[("src/b.rs", &tests)],
    ]);
    assert_eq!(pairs(&review_diff(&fx, "HEAD")), vec![]);
}

/// Wrapping a block the function already held (re-indenting it) adds lines
/// the diff counts, but no code the change wrote.
#[test]
fn reindenting_a_block_a_function_held_is_not_a_copy() {
    let body =
        "    let mut stmt = conn.prepare(\"SELECT id, name, score FROM rows WHERE active = 1\");
    let rows = stmt.query_map(|r| Row { id: r.get(0), name: r.get(1), score: r.get(2) });
    let kept: Vec<Row> = rows.into_iter().filter(|r| r.score > 10).collect();
    let total = kept.len();
";
    let before = format!("pub fn summarize(conn: &Conn) -> usize {{\n{body}    total\n}}\n");
    let indented: String = body.lines().map(|l| format!("    {l}\n")).collect();
    let after = format!(
        "pub fn summarize(conn: &Conn) -> usize {{\n    if conn.ready() {{\n{indented}        return total;\n    }}\n    0\n}}\n"
    );
    let fx = fixture(&[
        &[
            ("src/lib.rs", LIB),
            ("src/a.rs", LOAD),
            ("src/b.rs", &before),
            ("src/c.rs", OTHER),
        ],
        &[("src/b.rs", &after)],
    ]);
    assert_eq!(pairs(&review_diff(&fx, "HEAD")), vec![]);
}

/// A function past the HRR line cap never gets an embed vector; the review
/// says so instead of scoring it as if the embed channel had run.
#[test]
fn a_function_too_long_to_embed_is_reported_incomplete() {
    let long: String = format!(
        "{OTHER}\npub fn huge() -> usize {{\n{}    0\n}}\n",
        "    let _ = 1;\n".repeat(2_001)
    );
    let fx = fixture(&[
        &[("src/lib.rs", LIB), ("src/a.rs", LOAD), ("src/c.rs", &long)],
        &[("src/b.rs", &copy_named("fetch_rows"))],
    ]);
    let advisory = review_diff(&fx, "HEAD");
    assert!(advisory.error.is_none(), "{:?}", advisory.error);
    let incomplete = &advisory.report.incomplete;
    assert!(
        incomplete
            .iter()
            .any(|w| w.starts_with("embed line cap") && w.contains("huge")),
        "{incomplete:?}"
    );
    // The copy still fires on its shared runs.
    assert_eq!(advisory.report.findings.len(), 1);
}

/// A changed file with syntax errors is not classified from its partial
/// symbols: it is reported, not read as clean.
#[test]
fn a_changed_file_with_syntax_errors_is_reported_not_checked() {
    let broken = format!("{}\npub fn broken( {{\n", copy_named("fetch_rows"));
    let fx = fixture(&[
        &[("src/lib.rs", LIB), ("src/a.rs", LOAD), ("src/c.rs", OTHER)],
        &[("src/b.rs", &broken)],
    ]);
    let advisory = review_diff(&fx, "HEAD");
    assert!(advisory.error.is_none(), "{:?}", advisory.error);
    assert!(advisory.report.findings.is_empty());
    assert!(
        advisory
            .report
            .incomplete
            .iter()
            .any(|w| w.starts_with("src/b.rs: syntax errors")),
        "{:?}",
        advisory.report.incomplete
    );
}
