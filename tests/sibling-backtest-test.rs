//! The sibling-pattern back-test and noise ceiling (sutra/462, sutra/467) as
//! assertions (sutra/492). Each commit is reviewed where it sits in history,
//! with no checkout and an empty index: survivors must come from the commit's
//! own tree, so nothing in the current worktree or index can leak in.
//!
//! The sutra commits are this checkout's own history and must be present.
//! yojana and adityas/backend are read from their usual paths; when one is
//! missing its commits are reported as skipped. Numbers and labels:
//! `docs/sibling-pattern-backtest.md`.

use std::path::{Path, PathBuf};

use sutra::db::Db;
use sutra::parser::adapter::default_registry;
use sutra::tools::review;
use sutra::tools::sibling_pattern::{self, Budget, Controls, PatternClass, SiblingReport};

#[derive(Clone, Copy, PartialEq)]
enum Repo {
    Sutra,
    Yojana,
    Backend,
}

impl Repo {
    fn path(self) -> PathBuf {
        let sutra = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        match self {
            Self::Sutra => sutra,
            Self::Yojana => sutra.join("../yojana"),
            Self::Backend => {
                PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("adityas/backend")
            }
        }
    }
}

fn has_commit(repo: &Path, sha: &str) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "-e", &format!("{sha}^{{commit}}")])
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The report for `sha`'s own diff, or `None` when an external repo or commit
/// is not available here.
fn replay(repo: Repo, sha: &str) -> Option<SiblingReport> {
    let root = repo.path();
    if !has_commit(&root, sha) {
        assert!(
            repo != Repo::Sutra,
            "{sha} is missing from this checkout's history"
        );
        eprintln!("SKIPPED {sha}: not available in {}", root.display());
        return None;
    }
    let db_dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("backtest", db_dir.path()).unwrap();
    let scope = review::resolve_diff_entries(&root, sha).unwrap();
    let report = sibling_pattern::analyze(
        &db,
        &root,
        &scope,
        &default_registry(),
        Controls::default(),
        Budget::default(),
    )
    .unwrap();
    assert!(
        report.incomplete.is_empty(),
        "{sha}: {:?}",
        report.incomplete
    );
    Some(report)
}

/// `(file, line, enclosing symbol)`.
type Site = (&'static str, usize, &'static str);

/// The class of the finding that lists the survivor, if one does.
fn survivor_class(
    report: &SiblingReport,
    file: &str,
    line: usize,
    symbol: &str,
) -> Option<PatternClass> {
    report
        .findings
        .iter()
        .find(|f| {
            f.survivors.iter().any(|s| {
                s.file == file
                    && s.line == line
                    && s.symbol
                        .as_deref()
                        .is_some_and(|n| n.rsplit("::").next() == Some(symbol))
            })
        })
        .map(|f| f.class)
}

/// The first fix of each PAR pair flags the site of the later bug, and the
/// sites that later needed their own fixes.
#[test]
fn back_test_flags_each_known_site() {
    use PatternClass::{Rewritten, Wrapped};
    let cases: &[(Repo, &str, PatternClass, &[Site])] = &[
        (
            Repo::Sutra,
            "91fedc7", // sutra/280 → 283
            Rewritten,
            &[
                ("src/guard.rs", 407, "language_from_path"),
                ("src/tools/symbol_diff.rs", 539, "language_for_path"), // sutra/261
            ],
        ),
        (
            Repo::Sutra,
            "af68577", // sutra/438 → 441
            Wrapped,
            &[("src/tools/trend.rs", 589, "aggregate_categories")],
        ),
        (
            Repo::Sutra,
            "d70bf20", // sutra/308 → 459
            Wrapped,
            &[
                ("src/guard.rs", 765, "check_proposed_patterns"),
                (
                    "src/constraints/check.rs",
                    1109,
                    "partition_manifest_findings",
                ), // sutra/461
                ("src/constraints/check.rs", 1284, "check_pubspec_raw"), // sutra/461
            ],
        ),
        (
            Repo::Yojana,
            "0b3ff34", // yojana/42 → 47
            Rewritten,
            &[("src/tools/task.rs", 208, "json_array")],
        ),
    ];
    for &(repo, sha, class, sites) in cases {
        let Some(report) = replay(repo, sha) else {
            continue;
        };
        for &(file, line, symbol) in sites {
            assert_eq!(
                survivor_class(&report, file, line, symbol),
                Some(class),
                "{sha}: survivor {file}:{line} ({symbol}) in {:#?}",
                report.findings
            );
        }
    }
}

/// Additive fixes remove nothing, so the check is silent on them by
/// construction.
#[test]
fn back_test_is_silent_on_additive_fixes() {
    for (repo, sha) in [
        (Repo::Sutra, "ac25e6e"),
        (Repo::Sutra, "6e5d60e"),
        (Repo::Sutra, "723ac70"),
        (Repo::Backend, "a06a497"),
    ] {
        if let Some(report) = replay(repo, sha) {
            assert!(report.findings.is_empty(), "{sha}: {:#?}", report.findings);
        }
    }
}

/// The final, never-tuned noise sample (sample.py seed 99): 31 ordinary
/// commits. The frozen prototype reported 3 items there; production must not
/// do worse.
#[test]
fn seed_99_noise_ceiling() {
    const CEILING: usize = 3;
    let sample: &[(Repo, &[&str])] = &[
        (
            Repo::Sutra,
            &[
                "6576c69", "fa1867d", "b1fcc26", "5b5d52c", "6ff97b3", "bafae79", "3ec7483",
                "e7e8a0c", "dac2080", "302be1d", "49bbe74",
            ],
        ),
        (
            Repo::Yojana,
            &[
                "5fc5716", "bdc147f", "0b2b4a9", "16b954d", "0241b88", "ebe3192", "43d51d7",
                "93ea5e7", "c1c7a60", "e70980c",
            ],
        ),
        (
            Repo::Backend,
            &[
                "4fb76ce", "124f706", "7694284", "263c2fc", "514e866", "d7dc4a2", "db1aafa",
                "9fc2c0a", "a2725e7", "80b61dc",
            ],
        ),
    ];
    let mut items = Vec::new();
    for &(repo, shas) in sample {
        for &sha in shas {
            if let Some(report) = replay(repo, sha) {
                items.extend(report.findings.into_iter().map(|f| (sha, f)));
            }
        }
    }
    assert!(items.len() <= CEILING, "{} items: {items:#?}", items.len());
}
