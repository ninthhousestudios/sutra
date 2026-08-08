//! Integration coverage for the `.sutra/accepted.toml` freshness gate and the
//! guard/review parity it guarantees (sutra/308 — wiring of the sutra/303
//! portable-acceptance engine into the read/write surfaces).
//!
//! Two hazards are pinned here:
//!  1. **Migration preserves legacy DB waivers.** A repo whose acceptances still
//!     live only in the (gitignored) DB must not lose them when the DD review
//!     path first runs: `migrate_db_to_file` seeds the file BEFORE the cache is
//!     reprojected, or an absent-file reprojection wipes them.
//!  2. **The guard predicts the audit.** A hand-edited `accepted.toml` the server
//!     has not yet reprojected must be honored identically by the guard's
//!     read-only (`RawConn`) path and by the DD-backed review — both derive from
//!     the same file.

use sutra::constraints::check::{EvalScope, FactsSource, evaluate};
use sutra::db::Db;
use sutra::parser::adapter::default_registry;

/// A named `forbidden_dep` from `src/ui/*` to `src/db/*`, two files, and the
/// violating import. Returns the workspace dir, the open DB, and the two file ids.
fn violating_workspace() -> (tempfile::TempDir, Db, i64, i64) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();

    let rules_dir = dir.path().join(".sutra");
    std::fs::create_dir_all(&rules_dir).unwrap();
    std::fs::write(
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
    db.insert_import(
        f_view.id,
        "src/db/query.rs",
        Some(f_query.id),
        1,
        "use",
        None,
    )
    .unwrap();

    (dir, db, f_view.id, f_query.id)
}

fn constraint_id(dir: &std::path::Path) -> String {
    let (constraints, _) = sutra::rules::load_rules(dir).unwrap().all_constraints();
    constraints[0].id.to_string()
}

/// Hazard 1: a waiver that exists only in the DB (no file yet) is migrated into
/// `.sutra/accepted.toml` on the first DD review AND honored — not reprojected
/// away by an absent-file cache rebuild.
#[test]
fn legacy_db_waiver_is_migrated_to_file_and_honored() {
    let (dir, db, _, _) = violating_workspace();
    let cid = constraint_id(dir.path());

    // Seed the waiver in the DB only; no accepted.toml on disk yet.
    db.create_constraint_waiver(
        &cid,
        Some("ui-not-db"),
        "src/ui/view.rs",
        None,
        "legacy coupling",
        "josh",
    )
    .unwrap();
    let accepted_path = dir.path().join(".sutra/accepted.toml");
    assert!(!accepted_path.exists(), "no file before the first review");

    let registry = default_registry();
    let outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    // The violation is waived, not active — the legacy waiver survived the move.
    assert!(
        outcome
            .active
            .iter()
            .all(|f| f.constraint_kind != "forbidden_dep"),
        "migrated waiver must suppress the finding"
    );
    assert_eq!(
        outcome.waived.len(),
        1,
        "the forbidden_dep violation is waived"
    );

    // And the acceptance now lives in the version-controllable file.
    assert!(accepted_path.exists(), "migration wrote the file");
    let body = std::fs::read_to_string(&accepted_path).unwrap();
    assert!(
        body.contains("ui-not-db"),
        "waiver keyed by constraint name"
    );
    assert!(body.contains("legacy coupling"), "rationale preserved");
}

/// Hazard 3: a hand-edited `accepted.toml` no server pass has reprojected is
/// honored identically by the guard's read-only path and by the DD review — the
/// guard predicts exactly what the audit will report.
#[test]
fn hand_edited_file_honored_by_guard_and_review_alike() {
    let (dir, db, view_id, query_id) = violating_workspace();

    // Hand-write the acceptance; the DB cache is now stale (never projected).
    std::fs::write(
        dir.path().join(".sutra/accepted.toml"),
        "[[waiver]]\nconstraint = \"ui-not-db\"\nfile = \"src/ui/view.rs\"\n\
         rationale = \"hand edited\"\nby = \"josh\"\n",
    )
    .unwrap();

    let registry = default_registry();

    // Guard path FIRST, while the cache is still stale: a read-only connection
    // cannot reproject, so it must resolve the waiver straight from the file.
    let db_path = dir.path().join("test").join("index.db");
    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let edges = vec![(view_id, query_id)];
    let guard_outcome = evaluate(
        &FactsSource::RawConn(&conn),
        dir.path(),
        EvalScope::Edges {
            edges: &edges,
            externals: &[],
        },
        &registry,
    )
    .unwrap();

    assert!(
        guard_outcome
            .active
            .iter()
            .all(|f| f.constraint_kind != "forbidden_dep"),
        "guard must honor the hand-edited file waiver"
    );
    assert_eq!(
        guard_outcome.waived.len(),
        1,
        "guard reports the violation as waived, not active"
    );

    // Review path: same file, DD-backed. Reprojects then partitions.
    let review_outcome = evaluate(
        &FactsSource::DdBacked {
            db: &db,
            dd_engine: None,
        },
        dir.path(),
        EvalScope::Workspace,
        &registry,
    )
    .unwrap();

    assert!(
        review_outcome
            .active
            .iter()
            .all(|f| f.constraint_kind != "forbidden_dep"),
        "review must honor the same waiver the guard did"
    );
    assert_eq!(
        review_outcome.waived.len(),
        1,
        "guard and review agree: exactly one waived violation"
    );
}
