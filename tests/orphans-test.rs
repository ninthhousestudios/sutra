//! The orphans advisory end to end (sutra/483): a commit adds a symbol nothing
//! calls, or removes the last caller of one, and the review names it.

mod support;

use support::{Fixture, index, repo};
use sutra::parser::adapter::default_registry;
use sutra::tools::orphans::{self, Advisory, OrphanKind};
use sutra::tools::review;

const LIB: &str = "pub mod a;\npub mod b;\n";
const HELPER: &str = "pub fn helper() -> u32 {\n    1\n}\n";
const CALLER: &str = "pub fn caller() -> u32 {\n    crate::a::helper()\n}\n";
const NO_CALL: &str = "pub fn caller() -> u32 {\n    2\n}\n";

fn fixture(commits: &[&[(&str, &str)]]) -> Fixture {
    index(repo(commits), "orphans")
}

/// Review `diff` as `sutra check` does, recording firings under the sibling
/// check's review event.
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
    orphans::run_advisory(
        &fx.db,
        fx.root.path(),
        &scope,
        &registry,
        ("check", diff),
        sibling.patch(),
    )
}

fn names(advisory: &Advisory) -> Vec<(OrphanKind, &str, usize)> {
    assert!(advisory.error.is_none(), "{:?}", advisory.error);
    assert!(advisory.skipped.is_none(), "{:?}", advisory.skipped);
    assert!(
        advisory.report.incomplete.is_empty(),
        "{:?}",
        advisory.report.incomplete
    );
    advisory
        .report
        .findings
        .iter()
        .map(|f| (f.kind, f.symbol.as_str(), f.test_refs))
        .collect()
}

#[test]
fn added_symbol_with_no_caller_is_named() {
    let fx = fixture(&[
        &[
            ("src/lib.rs", LIB),
            ("src/a.rs", HELPER),
            ("src/b.rs", CALLER),
        ],
        &[(
            "src/a.rs",
            "pub fn helper() -> u32 {\n    1\n}\n\npub fn built_ahead() -> u32 {\n    3\n}\n\n\
             pub fn exercised_only() -> u32 {\n    4\n}\n\n\
             #[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        \
             assert_eq!(super::exercised_only(), 4);\n    }\n}\n",
        )],
    ]);
    let advisory = review_diff(&fx, "HEAD");
    assert_eq!(
        names(&advisory),
        vec![
            (OrphanKind::Added, "built_ahead", 0),
            (OrphanKind::Added, "exercised_only", 1),
        ]
    );
    let out = advisory.to_json();
    assert_eq!(out["findings"][0]["kind"], "added");
    assert_eq!(out["findings"][0]["file"], "src/a.rs");
    assert_eq!(out["findings"][0]["symbols"][0]["line"], 5);
}

#[test]
fn removing_the_last_caller_orphans_the_callee() {
    let fx = fixture(&[
        &[
            ("src/lib.rs", LIB),
            ("src/a.rs", HELPER),
            ("src/b.rs", CALLER),
        ],
        &[("src/b.rs", NO_CALL)],
    ]);
    assert_eq!(
        names(&review_diff(&fx, "HEAD")),
        vec![(OrphanKind::Orphaned, "helper", 0)]
    );
}

#[test]
fn a_callee_with_another_caller_is_not_orphaned() {
    let fx = fixture(&[
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\n"),
            ("src/a.rs", HELPER),
            ("src/b.rs", CALLER),
            (
                "src/c.rs",
                "pub fn other() -> u32 {\n    crate::a::helper()\n}\n",
            ),
        ],
        &[("src/b.rs", NO_CALL)],
    ]);
    assert_eq!(names(&review_diff(&fx, "HEAD")), vec![]);
}

#[test]
fn a_moved_function_is_not_added() {
    let fx = fixture(&[
        &[
            ("src/lib.rs", LIB),
            ("src/a.rs", HELPER),
            ("src/b.rs", CALLER),
        ],
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\n"),
            ("src/a.rs", ""),
            ("src/c.rs", HELPER),
            (
                "src/b.rs",
                "pub fn caller() -> u32 {\n    crate::c::helper()\n}\n",
            ),
        ],
    ]);
    assert_eq!(names(&review_diff(&fx, "HEAD")), vec![]);
}

#[test]
fn dart_structure_keeps_static_holders_and_platform_twins_live() {
    let fx = fixture(&[
        &[("lib/main.dart", "void main() {}\n")],
        &[
            (
                "lib/main.dart",
                "import 'iri.dart';\nimport 'load_io.dart' if (dart.library.js_interop) 'load_web.dart';\n\n\
                 void main() {\n  print(Iri.chart('x'));\n  load();\n}\n",
            ),
            (
                "lib/iri.dart",
                "final class Iri {\n  Iri._();\n\n  static String chart(String s) => s;\n}\n\n\
                 String unused() => 'no';\n",
            ),
            ("lib/load_io.dart", "void load() {}\n"),
            ("lib/load_web.dart", "void load() {}\n"),
        ],
    ]);
    assert_eq!(
        names(&review_diff(&fx, "HEAD")),
        vec![(OrphanKind::Added, "unused", 0)]
    );
}

#[test]
fn python_functions_used_as_values_or_decorators_are_live() {
    // sutra/515: a callback passed as an argument, held in a tuple or dict,
    // or applied as a decorator is referenced, not orphaned.
    let fx = fixture(&[
        &[("app.py", "def main():\n    pass\n")],
        &[(
            "app.py",
            "def _style():\n    pass\n\n\
             def _fetch():\n    pass\n\n\
             def _years():\n    pass\n\n\
             def _batched(f):\n    return f\n\n\
             def _unused():\n    pass\n\n\
             PROVIDERS = (_fetch,)\n\
             COLUMNS = {\"years\": _years}\n\n\
             @_batched\n\
             def main():\n    register(PROVIDERS, COLUMNS, _style)\n",
        )],
    ]);
    assert_eq!(
        names(&review_diff(&fx, "HEAD")),
        vec![(OrphanKind::Added, "_unused", 0)]
    );
}

#[test]
fn a_historical_commit_is_skipped_not_clean() {
    let fx = fixture(&[
        &[
            ("src/lib.rs", LIB),
            ("src/a.rs", HELPER),
            ("src/b.rs", CALLER),
        ],
        &[("src/b.rs", NO_CALL)],
        &[("src/lib.rs", "pub mod a;\npub mod b;\n// later\n")],
    ]);
    let advisory = review_diff(&fx, "HEAD~1");
    assert!(advisory.report.findings.is_empty());
    assert!(
        advisory
            .skipped
            .as_deref()
            .is_some_and(|s| s.contains("worktree")),
        "{:?}",
        advisory.skipped
    );
    assert_eq!(
        advisory.to_json()["skipped"],
        advisory.skipped.as_deref().unwrap_or("")
    );
}

#[test]
fn findings_are_logged_once_per_review_event() {
    let fx = fixture(&[
        &[
            ("src/lib.rs", LIB),
            ("src/a.rs", HELPER),
            ("src/b.rs", CALLER),
        ],
        &[("src/b.rs", NO_CALL)],
    ]);
    for _ in 0..2 {
        let advisory = review_diff(&fx, "HEAD");
        assert!(
            advisory.firing_log_error.is_none(),
            "{:?}",
            advisory.firing_log_error
        );
    }
    let rows = fx.db.firings(Some(orphans::MECHANISM), None).unwrap();
    assert_eq!(rows.len(), 1, "a repeated review must not re-count");
    assert_eq!(rows[0].finding_kind, "orphaned");
    assert_eq!(rows[0].finding_key, "helper");
    assert_eq!(rows[0].file_path, "src/a.rs");
    assert_eq!(rows[0].snippet.as_deref(), Some("pub fn helper() -> u32 {"));
}

const BUILT_AHEAD: &str =
    "pub fn helper() -> u32 {\n    1\n}\n\npub fn built_ahead() -> u32 {\n    3\n}\n";

fn incomplete_names(advisory: &Advisory, path: &str) -> bool {
    advisory.report.incomplete.iter().any(|i| i.contains(path))
}

#[test]
fn a_head_review_with_a_dirty_file_outside_the_diff_is_incomplete() {
    let root = repo(&[
        &[
            ("src/lib.rs", LIB),
            ("src/a.rs", HELPER),
            ("src/b.rs", CALLER),
        ],
        &[("src/a.rs", BUILT_AHEAD)],
    ]);
    // Uncommitted: the only caller of `built_ahead` is not in the reviewed commit.
    support::write(
        root.path(),
        "src/b.rs",
        "pub fn caller() -> u32 {\n    crate::a::helper() + crate::a::built_ahead()\n}\n",
    );
    let fx = index(root, "orphans");
    let advisory = review_diff(&fx, "HEAD");
    assert!(
        incomplete_names(&advisory, "src/b.rs"),
        "{:?}",
        advisory.report.incomplete
    );
    assert!(advisory.to_json()["incomplete"].is_array());
}

#[test]
fn a_staged_review_with_an_untracked_caller_is_incomplete() {
    let root = repo(&[&[
        ("src/lib.rs", LIB),
        ("src/a.rs", HELPER),
        ("src/b.rs", CALLER),
    ]]);
    support::write(root.path(), "src/a.rs", BUILT_AHEAD);
    support::git(root.path(), &["add", "src/a.rs"]);
    support::write(
        root.path(),
        "src/c.rs",
        "pub fn other() -> u32 {\n    crate::a::built_ahead()\n}\n",
    );
    let fx = index(root, "orphans");
    let advisory = review_diff(&fx, "staged");
    assert!(
        incomplete_names(&advisory, "src/c.rs"),
        "{:?}",
        advisory.report.incomplete
    );
}

#[test]
fn a_clean_staged_review_is_complete() {
    let root = repo(&[&[
        ("src/lib.rs", LIB),
        ("src/a.rs", HELPER),
        ("src/b.rs", CALLER),
    ]]);
    support::write(root.path(), "src/a.rs", BUILT_AHEAD);
    support::git(root.path(), &["add", "src/a.rs"]);
    let fx = index(root, "orphans");
    assert_eq!(
        names(&review_diff(&fx, "staged")),
        vec![(OrphanKind::Added, "built_ahead", 0)]
    );
}

#[test]
fn an_unqualified_removal_does_not_orphan_an_already_dead_namesake() {
    let fx = fixture(&[
        &[
            ("src/lib.rs", "pub mod a;\npub mod b;\npub mod c;\n"),
            ("src/a.rs", HELPER),
            // Dead before the diff; nothing ever called it.
            ("src/c.rs", "pub fn helper() -> u32 {\n    9\n}\n"),
            (
                "src/b.rs",
                "use crate::a::helper;\n\npub fn caller() -> u32 {\n    helper()\n}\n",
            ),
        ],
        &[("src/b.rs", NO_CALL)],
    ]);
    let advisory = review_diff(&fx, "HEAD");
    let files: Vec<_> = advisory
        .report
        .findings
        .iter()
        .map(|f| (f.kind, f.symbol.as_str(), f.file.as_str()))
        .collect();
    // The price of attributing by name without the base tree: `a::helper`,
    // the one this diff did orphan, is dropped with its namesake. An
    // ambiguous name reports nothing rather than a symbol that was already dead.
    assert_eq!(files, vec![]);
}

#[test]
fn an_unused_private_dart_variable_fires_and_a_public_one_does_not() {
    let fx = fixture(&[
        &[("lib/main.dart", "void main() {}\n")],
        &[(
            "lib/main.dart",
            "const _unusedPrivate = 1;\nconst unusedPublic = 2;\n\nvoid main() {}\n",
        )],
    ]);
    assert_eq!(
        names(&review_diff(&fx, "HEAD")),
        vec![(OrphanKind::Added, "_unusedPrivate", 0)]
    );
}
