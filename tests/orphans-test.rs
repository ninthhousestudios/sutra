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
