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
use std::process::Command;
use std::time::Instant;

use sutra::config::Config;
use sutra::db::Db;
use sutra::git::head_commit;
use sutra::health::BiomarkerKind;
use sutra::health::evidence::{InputFailure, MissingReason, ProducerOutcome};
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

// --- git-history fixture + outcome helpers (git-history real-path suite) ---

/// Run a git command in `root`, optionally pinning committer+author date to a
/// fixed unix timestamp, and assert success.
fn git(root: &Path, args: &[&str], date: Option<i64>) {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    if let Some(ts) = date {
        let val = format!("@{ts} +0000");
        cmd.env("GIT_COMMITTER_DATE", &val)
            .env("GIT_AUTHOR_DATE", &val);
    }
    let out = cmd.output().expect("git spawn");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_init(root: &Path) {
    git(root, &["init", "-q"], None);
    git(root, &["config", "user.email", "test@example.com"], None);
    git(root, &["config", "user.name", "Test"], None);
    // Deterministic default branch regardless of the host git's init.defaultBranch.
    git(root, &["symbolic-ref", "HEAD", "refs/heads/main"], None);
}

/// Stage everything and commit with a pinned committer timestamp.
fn git_commit(root: &Path, ts: i64) {
    git(root, &["add", "-A"], None);
    // --no-verify: skip any inherited pre-commit hook (e.g. a global rustfmt
    // gate) — these seed files are fixtures, not project source.
    git(
        root,
        &["commit", "-q", "--no-verify", "-m", "seed"],
        Some(ts),
    );
}

fn head_sha(root: &Path) -> String {
    head_commit(root).unwrap().expect("resolved HEAD")
}

/// A `fixture` whose workspace root is a real git repo with the given files
/// committed in two passes, each touching every file, so any indexed pair
/// co-changes with jaccard 1.0. Committer timestamps sit comfortably inside the
/// trailing 90-day window, so history ingests as `Loaded`.
fn git_fixture(id: &str, files: &[(&str, &str)]) -> Fixture {
    let fx = fixture(id, files);
    git_init(&fx.ws.root);
    let now = chrono::Utc::now().timestamp();
    git_commit(&fx.ws.root, now - 7200);
    // A second pass touching every file, so the history is plural and the
    // co-change signal survives the eligible-commit fan-out filter cleanly.
    for (rel, _) in files {
        let p = fx.ws.root.join(rel);
        let mut src = std::fs::read_to_string(&p).unwrap();
        src.push_str("// seed touch\n");
        std::fs::write(&p, src).unwrap();
    }
    git_commit(&fx.ws.root, now - 3600);
    fx
}

/// The published run's outcomes for one producer, across all files.
fn producer_outcomes(db: &Db, producer: BiomarkerKind) -> Vec<ProducerOutcome> {
    let run = db
        .load_current_health_run()
        .unwrap()
        .expect("a published health run");
    run.outcomes
        .iter()
        .filter(|o| o.producer == producer)
        .map(|o| o.outcome)
        .collect()
}

/// Assert every per-file outcome for `producer` is `Complete` — i.e. the git
/// producer saw `Loaded` history, not `Missing(NoHistory)`/`Unsupported`.
fn assert_all_complete(db: &Db, producer: BiomarkerKind) {
    let outcomes = producer_outcomes(db, producer);
    assert!(!outcomes.is_empty(), "{producer:?} must stage an outcome");
    for outcome in &outcomes {
        assert!(
            matches!(outcome, ProducerOutcome::Complete { .. }),
            "{producer:?} must be Complete with Loaded history, got {outcome:?}"
        );
    }
}

/// Total staged finding count across files for one producer.
fn producer_finding_total(db: &Db, producer: BiomarkerKind) -> usize {
    producer_outcomes(db, producer)
        .iter()
        .map(|o| match o {
            ProducerOutcome::Complete { finding_count } => *finding_count,
            _ => 0,
        })
        .sum()
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

// --- Git-history real path: git producers recompute on demand refresh ---
//
// The tests above use non-git temp workspaces, so history ingests as
// Empty/Unsupported and every git producer is Missing(NoHistory). These drive a
// real repo through the same full parse → edit → incremental → demand refresh
// path, closing the git-history observational gap the health-evidence contract
// enumerates: HiddenCoupling/BlastRadiusChurn depend on Loaded history, and
// HiddenCoupling additionally on the current static graph edges.

/// The pair of indexed files every git test co-changes. They start with no
/// cross-file reference, so the co-change lacks a static edge → HiddenCoupling
/// fires. Mirrors the resolving import construct proven by the rollups test.
const GIT_FILES: [(&str, &str); 2] = [
    ("src/lib.rs", "pub fn hello() -> i32 { 1 }\n"),
    ("src/util.rs", "pub fn greet() -> i32 { 2 }\n"),
];

/// Build a workspace that is a *shallow* clone (`--depth 1`) of a THREE-commit
/// origin repo. Returns the fixture (whose workspace root is the clone) plus the
/// origin tempdir the caller must keep alive. The clone's object graph is
/// truncated at a shallow boundary, so history is incomplete: even a successful
/// `git log` over the window cannot establish that every qualifying commit is
/// present (health-evidence contract; sutra/427). Three origin commits leave the
/// clone still shallow after a single `--deepen 1`, so a deepening test keeps a
/// truncated boundary.
fn shallow_git_fixture(id: &str) -> (Fixture, tempfile::TempDir) {
    let origin = tempfile::tempdir().unwrap();
    for (rel, contents) in GIT_FILES {
        let full = origin.path().join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, contents).unwrap();
    }
    git_init(origin.path());
    let now = chrono::Utc::now().timestamp();
    // Three commits, each touching every file, all inside the trailing window.
    for (i, ts) in [now - 10_800, now - 7_200, now - 3_600]
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            for (rel, _) in GIT_FILES {
                let p = origin.path().join(rel);
                let mut src = std::fs::read_to_string(&p).unwrap();
                src.push_str("// seed touch\n");
                std::fs::write(&p, src).unwrap();
            }
        }
        git_commit(origin.path(), ts);
    }

    // A depth-1 clone of the origin over file:// (a plain local path ignores
    // --depth): the working tree checks out, the ancestry is truncated.
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    let origin_url = format!("file://{}", origin.path().display());
    git(
        root.path(),
        &[
            "clone",
            "-q",
            "--depth",
            "1",
            &origin_url,
            work.to_str().unwrap(),
        ],
        None,
    );

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry(id, work);
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    (
        Fixture {
            _root: root,
            _db_dir: db_dir,
            ws,
            config,
            db,
        },
        origin,
    )
}

/// Assert every per-file outcome for `producer` is `Missing(HistoryIncomplete)`
/// — the shallow-clone contract: the git producer saw an incomplete history, not
/// `Complete` (which would silently treat truncated history as measured).
fn assert_all_history_incomplete(db: &Db, producer: BiomarkerKind) {
    let outcomes = producer_outcomes(db, producer);
    assert!(!outcomes.is_empty(), "{producer:?} must stage an outcome");
    for outcome in &outcomes {
        assert!(
            matches!(
                outcome,
                ProducerOutcome::Missing(MissingReason::Failed(InputFailure::HistoryIncomplete))
            ),
            "{producer:?} over a shallow clone must be Missing(HistoryIncomplete), got {outcome:?}"
        );
    }
}

// --- AC1: a full parse WITH history marks the git producers Complete ---

#[test]
fn full_parse_with_history_marks_git_producers_complete() {
    let fx = git_fixture("git-complete", &GIT_FILES);
    full_parse(&fx);

    // Loaded history reaches every git producer: Complete, not Missing(NoHistory)
    // as it would be in the non-git workspaces the sibling tests use.
    assert_all_complete(&fx.db, BiomarkerKind::HiddenCoupling);
    assert_all_complete(&fx.db, BiomarkerKind::BlastRadiusChurn);
    assert_all_complete(&fx.db, BiomarkerKind::CoChangeScatter);
    assert_all_complete(&fx.db, BiomarkerKind::ChangeEntropy);

    // lib/util co-change with no static edge → HiddenCoupling actually fires,
    // proving the ingested history reached the producer (Complete { n>0 }).
    assert!(
        producer_finding_total(&fx.db, BiomarkerKind::HiddenCoupling) > 0,
        "co-changing files with no import edge must yield a hidden-coupling finding"
    );
}

// --- AC2: comment edit at unchanged HEAD re-ingests history, still Complete ---

#[test]
fn comment_edit_at_unchanged_head_keeps_git_producers_complete() {
    let fx = git_fixture("git-comment", &GIT_FILES);
    full_parse(&fx);
    let head = head_sha(&fx.ws.root);
    let coupling0 = producer_finding_total(&fx.db, BiomarkerKind::HiddenCoupling);
    assert!(coupling0 > 0, "baseline hidden coupling must be present");

    // A comment-only, UNCOMMITTED edit: HEAD (and the commit set) is unchanged.
    let mut src = std::fs::read_to_string(fx.ws.root.join("src/util.rs")).unwrap();
    src.push_str("// trailing comment, no semantic or history change\n");
    std::fs::write(fx.ws.root.join("src/util.rs"), &src).unwrap();

    // Generation bump → the demand refresh must rebuild and re-ingest history
    // against the same pinned HEAD + absolute cutoff, then republish.
    incremental_reparse(&fx);
    let out = demand_refresh(&fx);
    assert!(
        matches!(out, DemandOutcome::Refreshed(RefreshResult::Published(_))),
        "a generation bump must force a rebuild+republish, got {out:?}"
    );

    // HEAD really did not move: the refresh re-ingested against the SAME commit.
    assert_eq!(head_sha(&fx.ws.root), head, "HEAD must be unchanged");

    // Git producers stay Complete (not spuriously NoHistory/Missing), and with
    // both history and edges unchanged the hidden-coupling signal is identical.
    assert_all_complete(&fx.db, BiomarkerKind::HiddenCoupling);
    assert_all_complete(&fx.db, BiomarkerKind::BlastRadiusChurn);
    assert_eq!(
        producer_finding_total(&fx.db, BiomarkerKind::HiddenCoupling),
        coupling0,
        "comment-only edit changes no history and no static edges"
    );
}

// --- AC3: a static-edge change at unchanged HEAD flips HiddenCoupling ---

#[test]
fn static_edge_change_at_unchanged_head_flips_hidden_coupling() {
    let fx = git_fixture("git-edge", &GIT_FILES);
    full_parse(&fx);
    let head = head_sha(&fx.ws.root);
    assert!(
        producer_finding_total(&fx.db, BiomarkerKind::HiddenCoupling) > 0,
        "baseline: hidden coupling present (co-change, no static edge)"
    );

    // Introduce an import edge util -> lib in the WORKING TREE only (HEAD
    // unchanged), so the static graph now explains the co-change.
    std::fs::write(
        fx.ws.root.join("src/util.rs"),
        "use crate::hello;\npub fn greet() -> i32 { hello() + 2 }\n",
    )
    .unwrap();

    incremental_reparse(&fx);
    let out = demand_refresh(&fx);
    assert!(
        matches!(out, DemandOutcome::Refreshed(RefreshResult::Published(_))),
        "a graph-edge change must force a rebuild+republish, got {out:?}"
    );
    assert_eq!(head_sha(&fx.ws.root), head, "HEAD must be unchanged");

    // HiddenCoupling depends on history AND current edges: the same commit
    // history still ingests as Loaded (producer Complete), but the new static
    // edge over the co-changing pair suppresses the finding.
    assert_all_complete(&fx.db, BiomarkerKind::HiddenCoupling);
    assert_eq!(
        producer_finding_total(&fx.db, BiomarkerKind::HiddenCoupling),
        0,
        "a static edge over the co-changing pair must clear the hidden-coupling finding"
    );
}

// --- Shallow clone: history is incomplete, never Complete (sutra/427) ---

#[test]
fn shallow_clone_marks_git_producers_incomplete_not_complete() {
    let (fx, _origin) = shallow_git_fixture("git-shallow");
    full_parse(&fx);

    // The clone is shallow: HEAD resolves and the tip commit sits in the window,
    // but the truncated object graph means completeness of the requested range
    // cannot be positively established. Every git producer must therefore be
    // partial (Missing(HistoryIncomplete)), NOT Complete — the contract deviation
    // this task closes.
    assert_all_history_incomplete(&fx.db, BiomarkerKind::HiddenCoupling);
    assert_all_history_incomplete(&fx.db, BiomarkerKind::BlastRadiusChurn);
    assert_all_history_incomplete(&fx.db, BiomarkerKind::CoChangeScatter);
    assert_all_history_incomplete(&fx.db, BiomarkerKind::ChangeEntropy);
}

// --- Deepening at unchanged HEAD re-ingests, does not reuse stale evidence ---

#[test]
fn deepening_a_shallow_clone_at_unchanged_head_re_ingests() {
    let (fx, _origin) = shallow_git_fixture("git-deepen");
    full_parse(&fx);
    let head = head_sha(&fx.ws.root);

    // A clean demand request over a still-shallow clone never reuses: the history
    // is unconfirmed-complete, so the refresh always re-ingests and republishes.
    let out = demand_refresh(&fx);
    assert!(
        matches!(out, DemandOutcome::Refreshed(RefreshResult::Published(_))),
        "a shallow clone's unconfirmed history must re-ingest, not reuse, got {out:?}"
    );

    // Deepen the clone: HEAD is unchanged, but the shallow boundary set moves and
    // newly-accessible qualifying ancestors appear. The old is-shallow-boolean
    // fingerprint left the stamp unchanged here, so demand refresh reused stale
    // evidence. It must re-ingest.
    git(&fx.ws.root, &["fetch", "-q", "--deepen", "1"], None);
    assert_eq!(head_sha(&fx.ws.root), head, "deepening must not move HEAD");

    let out = demand_refresh(&fx);
    assert!(
        matches!(out, DemandOutcome::Refreshed(RefreshResult::Published(_))),
        "deepening at unchanged HEAD must re-ingest (publish), not reuse, got {out:?}"
    );

    // Deepening by one still leaves the three-commit origin truncated by one, so
    // the range is still incomplete — partial, not silently healed to Complete.
    assert_all_history_incomplete(&fx.db, BiomarkerKind::HiddenCoupling);
}

// --- sutra/418: snapshot completeness through the production writer ---

/// Assert every row of `snapshot_id` recorded its completeness (never the
/// legacy `Unknown`) and that it matches what the scorer computed for the file.
fn assert_snapshot_completeness_matches_scorer(db: &Db, snapshot_id: i64) {
    use sutra::db::SnapshotCompleteness;
    let scored = sutra::health::scoring::score_workspace(db).unwrap();
    let rows = db.snapshot_file_scores(snapshot_id).unwrap();
    assert!(!rows.is_empty(), "snapshot must carry per-file rows");
    for row in &rows {
        let sf = scored
            .file_scores
            .iter()
            .find(|s| s.file_id == row.file_id)
            .expect("every indexed file is scored");
        let mut expected_missing: Vec<String> = sf
            .missing
            .iter()
            .map(|m| m.biomarker.as_str().to_string())
            .collect();
        // The writer stores the set sorted; the scorer's order is unstable.
        expected_missing.sort_unstable();
        let expected = if expected_missing.is_empty() {
            SnapshotCompleteness::Complete
        } else {
            SnapshotCompleteness::Partial
        };
        assert_eq!(
            row.completeness, expected,
            "{}: completeness",
            row.file_path
        );
        assert_eq!(
            row.missing_biomarkers, expected_missing,
            "{}: missing",
            row.file_path
        );
    }
}

#[test]
fn full_parse_snapshot_round_trips_completeness_through_trend() {
    // A git repo with no commits → NoHistory → git biomarkers are worst-cased,
    // so the production writer must persist a genuinely partial score.
    let fx = fixture("snap-completeness", &[("src/deep.rs", DEEP_SRC)]);
    git_init(&fx.ws.root);

    full_parse(&fx);
    let first = fx.db.latest_snapshots(1).unwrap()[0].id;
    assert_snapshot_completeness_matches_scorer(&fx.db, first);
    let row = &fx.db.snapshot_file_scores(first).unwrap()[0];
    assert_eq!(
        row.completeness,
        sutra::db::SnapshotCompleteness::Partial,
        "NoHistory must worst-case git biomarkers into a partial score"
    );

    // Unchanged second parse → NoChanges copy-forward, the other production path.
    full_parse(&fx);
    let snaps = fx.db.latest_snapshots(2).unwrap();
    assert_eq!(snaps.len(), 2, "second parse records a checkpoint");
    assert_ne!(snaps[0].id, first);
    assert_snapshot_completeness_matches_scorer(&fx.db, snaps[0].id);

    // History exposes completeness on every entry.
    let history = sutra::tools::trend::handle(
        &fx.db,
        &sutra::tools::trend::TrendArgs {
            workspace: String::new(),
            from: None,
            to: None,
            path: Some("src/deep.rs".into()),
            limit: None,
        },
    )
    .unwrap();
    let entries = history["snapshots"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    for e in entries {
        assert_eq!(e["completeness"], "partial");
        assert_eq!(e["partial"], true);
        assert_eq!(
            e["missing_biomarkers"],
            serde_json::json!(row.missing_biomarkers)
        );
    }

    // Comparison: identical partial observations are no change, not incomparable.
    let cmp = sutra::tools::trend::handle(
        &fx.db,
        &sutra::tools::trend::TrendArgs {
            workspace: String::new(),
            from: None,
            to: None,
            path: None,
            limit: None,
        },
    )
    .unwrap();
    for bucket in ["improved", "degraded", "incomparable"] {
        assert!(
            cmp["files"][bucket].as_array().unwrap().is_empty(),
            "{bucket}: {}",
            cmp["files"][bucket]
        );
    }
    assert_eq!(cmp["aggregate_comparison"]["reason"], "incomplete_evidence");
    assert!(cmp["deltas"]["health_score"].is_null());
    assert_eq!(cmp["completeness"]["to"]["partial"], 1);
    assert_eq!(cmp["completeness"]["to"]["unknown"], 0);
}

#[test]
fn unchanged_parse_recomputes_an_unknown_completeness_snapshot() {
    // Upgrade case: the latest checkpoint's rows never recorded completeness
    // (pre-0076 / pre-418 atomic writer). An unchanged-source parse must not copy
    // that Unknown forward, or trend stays incomparable indefinitely.
    let fx = fixture("snap-unknown-upgrade", &[("src/deep.rs", DEEP_SRC)]);
    full_parse(&fx);
    let legacy = fx.db.latest_snapshots(1).unwrap()[0].id;
    fx.db
        .conn_for_test()
        .execute(
            "UPDATE health_snapshot_files SET partial = 0, completeness_recorded = 0
             WHERE snapshot_id = ?1",
            [legacy],
        )
        .unwrap();
    assert!(
        fx.db
            .snapshot_file_scores(legacy)
            .unwrap()
            .iter()
            .all(|r| r.completeness == sutra::db::SnapshotCompleteness::Unknown)
    );

    full_parse(&fx);
    let latest = fx.db.latest_snapshots(1).unwrap()[0].id;
    assert_ne!(latest, legacy, "unchanged parse records a checkpoint");
    assert_snapshot_completeness_matches_scorer(&fx.db, latest);
}
