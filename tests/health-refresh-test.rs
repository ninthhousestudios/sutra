//! Real-path regression for the demand health-refresh core and its acquiring
//! adapter (sutra/421 Wave E, governed by docs/health-evidence-contract.md).
//!
//! These drive the actual full parse → edit → incremental reparse → demand
//! refresh path (not manually-seeded rows), so they exercise the invariants the
//! contract turns on: an incremental reparse bumps the graph generation (via
//! resolution) and skips rollups, so a stale run must rebuild rollups + history
//! and republish; a clean request then reuses; and no health refresh ever records
//! a checkpoint or replaces the review baseline.

use std::path::{Path, PathBuf};
use std::time::Instant;

use sutra::config::Config;
use sutra::db::Db;
use sutra::health::refresh::{DemandOutcome, RefreshResult, refresh_acquiring};
use sutra::parser::adapter::default_registry;
use sutra::pipeline;
use sutra::workspace::WorkspaceEntry;

/// A function nested deeply enough (> 4) to emit a NestedComplexity finding.
const DEEP_SRC: &str = "\
pub fn deep(x: i32) -> i32 {
    if x > 0 {
        if x > 1 {
            if x > 2 {
                if x > 3 {
                    if x > 4 {
                        return x;
                    }
                }
            }
        }
    }
    0
}
";

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

/// Drive the query-path incremental reparse directly (resolves refs → bumps the
/// graph generation, but does NOT recompute rollups).
fn incremental_reparse(fx: &Fixture) -> pipeline::ParseSnapshot {
    let (_, drift) = sutra::freshness::workspace_drift(&fx.db, &fx.ws.root, &fx.ws.languages);
    let drift = drift.expect("baseline must exist for an incremental reparse");
    let registry = default_registry();
    pipeline::parse_incremental(&fx.ws, &fx.db, &fx.config, &registry, &drift).unwrap()
}

fn demand_refresh(fx: &Fixture) -> DemandOutcome {
    let now = chrono::Utc::now().timestamp();
    refresh_acquiring(&fx.config, &fx.ws.id, &fx.db, &fx.ws.root, now).unwrap()
}

fn snapshot_count(db: &Db) -> usize {
    db.latest_snapshots(1000).unwrap().len()
}

fn finding_count(db: &Db, rel: &str) -> usize {
    let fid = db.file_by_path(rel).unwrap().expect("file indexed").id;
    db.get_health_findings(Some(fid), None).unwrap().len()
}

fn total_blast_radius(db: &Db) -> i64 {
    db.all_files().unwrap().iter().map(|f| f.blast_radius).sum()
}

// --- AC1 + AC6: reuse after a full parse; refresh records no checkpoint ---

#[test]
fn full_parse_publishes_and_a_clean_demand_request_reuses() {
    let fx = fixture("reuse", &[("src/deep.rs", DEEP_SRC)]);

    let t = Instant::now();
    full_parse(&fx);
    let cold = t.elapsed();

    // The full parse published a run AND recorded exactly one checkpoint.
    let snaps_after_parse = snapshot_count(&fx.db);
    assert_eq!(snaps_after_parse, 1, "full parse records one checkpoint");
    assert!(
        fx.db.load_current_health_run().unwrap().is_some(),
        "full parse must publish a health run"
    );

    // A clean demand refresh reuses the retained run untouched...
    let t = Instant::now();
    let out = demand_refresh(&fx);
    let warm = t.elapsed();
    assert!(
        matches!(out, DemandOutcome::Refreshed(RefreshResult::Reused(_))),
        "unchanged inputs must reuse, got {out:?}"
    );

    // ...and NEVER records a checkpoint (contract: health refresh is not a parse).
    assert_eq!(
        snapshot_count(&fx.db),
        snaps_after_parse,
        "demand refresh must not record a checkpoint"
    );

    // AC6 cost record (visible with --nocapture).
    eprintln!("[cost] cold_full_parse={cold:?} warm_reuse={warm:?}");
}

// --- AC1: comment-only edit → rebuild recomputes original debt → then reuse ---

#[test]
fn comment_edit_recomputes_original_debt_then_next_request_reuses() {
    let fx = fixture("comment", &[("src/deep.rs", DEEP_SRC)]);
    full_parse(&fx);

    let debt0 = finding_count(&fx.db, "src/deep.rs");
    assert!(debt0 > 0, "deep nesting must yield at least one finding");
    let snaps = snapshot_count(&fx.db);

    // A comment-only edit changes bytes but not the symbol/nesting shape.
    let mut src = DEEP_SRC.to_string();
    src.push_str("// a trailing comment, no semantic change\n");
    std::fs::write(fx.ws.root.join("src/deep.rs"), &src).unwrap();

    // Incremental reparse bumps the generation via resolution → the run is stale.
    let t = Instant::now();
    incremental_reparse(&fx);
    let out = demand_refresh(&fx);
    let edited = t.elapsed();
    assert!(
        matches!(out, DemandOutcome::Refreshed(RefreshResult::Published(_))),
        "a generation bump must force a rebuild+republish, got {out:?}"
    );

    // The recomputed debt equals the original (the comment changed nothing real).
    assert_eq!(
        finding_count(&fx.db, "src/deep.rs"),
        debt0,
        "comment-only edit must restore identical debt"
    );

    // The following clean request reuses the freshly-published run.
    let out2 = demand_refresh(&fx);
    assert!(
        matches!(out2, DemandOutcome::Refreshed(RefreshResult::Reused(_))),
        "second clean request must reuse, got {out2:?}"
    );

    // Neither refresh recorded a checkpoint.
    assert_eq!(
        snapshot_count(&fx.db),
        snaps,
        "demand refresh must never record a checkpoint"
    );
    eprintln!("[cost] edited_rebuild={edited:?}");
}

// --- AC2: demand refresh rebuilds rollups that incremental parse leaves stale ---

#[test]
fn demand_refresh_recomputes_rollups_that_incremental_parse_skips() {
    let fx = fixture(
        "rollups",
        &[
            ("src/lib.rs", "pub fn hello() -> i32 { 1 }\n"),
            ("src/util.rs", "pub fn greet() -> i32 { 2 }\n"),
        ],
    );
    full_parse(&fx);
    let blast_full = total_blast_radius(&fx.db);

    // Introduce an import edge: util now depends on lib::hello.
    std::fs::write(
        fx.ws.root.join("src/util.rs"),
        "use crate::hello;\npub fn greet() -> i32 { hello() + 2 }\n",
    )
    .unwrap();

    // Incremental reparse resolves the new edge but does NOT recompute rollups.
    incremental_reparse(&fx);
    let blast_incremental = total_blast_radius(&fx.db);
    assert_eq!(
        blast_incremental, blast_full,
        "incremental parse must leave blast-radius rollups stale"
    );

    // The demand refresh rebuilds rollups over the current resolved graph, so the
    // new dependency is reflected — a fact BlastRadiusChurn depends on (AC2).
    let out = demand_refresh(&fx);
    assert!(
        matches!(out, DemandOutcome::Refreshed(RefreshResult::Published(_))),
        "stale inputs must rebuild, got {out:?}"
    );
    let blast_demand = total_blast_radius(&fx.db);
    assert_ne!(
        blast_demand, blast_incremental,
        "demand refresh must recompute rollups the incremental parse skipped"
    );
}

// --- AC4: a full reparse repairs a stale health run (same-content backlog) ---

#[test]
fn full_reparse_repairs_a_stale_health_run() {
    let fx = fixture("repair", &[("src/deep.rs", DEEP_SRC)]);
    full_parse(&fx);

    // Edit + incremental → the published run is now stale (generation moved).
    let mut src = DEEP_SRC.to_string();
    src.push_str("// comment\n");
    std::fs::write(fx.ws.root.join("src/deep.rs"), &src).unwrap();
    incremental_reparse(&fx);

    // A FULL reparse republishes the run through the shared locked core (its
    // post-parse publication), repairing the backlog without a demand refresh.
    full_parse(&fx);

    // The repaired run is current, so a demand refresh reuses it.
    let out = demand_refresh(&fx);
    assert!(
        matches!(out, DemandOutcome::Refreshed(RefreshResult::Reused(_))),
        "a full reparse must leave the run current, got {out:?}"
    );
}
