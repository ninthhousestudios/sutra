//! Locked health-evidence refresh core (sutra/415 Wave B).
//!
//! One code path publishes an immutable health run for both callers the contract
//! recognises (`docs/health-evidence-contract.md`, "Publication and consumers"):
//! the full parse (already holds the parse coordinator + flock, calls the
//! already-locked [`publish_run`]) and the demand refresh (the acquiring adapter
//! mints a [`HealthSession`] and calls [`refresh`]). Sharing the producers and
//! the staging means full parse and on-demand agree on findings/completeness for
//! equivalent inputs (AC3) rather than reimplementing per-file.
//!
//! The core never acquires the coordinator (no recursive lock) — the caller owns
//! the exclusive scope and passes it in as a [`HealthSession`] witness. All
//! mutation (rollup rebuild, history ingestion, findings replace, run publish)
//! happens under that scope; publication verifies the graph generation in one
//! SQLite transaction ([`Db::publish_health_run`]) and returns
//! [`RefreshResult::InputsChanged`] rather than a complete stamp over mixed
//! inputs on a race.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use tracing::warn;

use crate::db::{CommitRow, Db, HealthFindingRow};
use crate::error::Result;
use crate::git;
use crate::graph;
use crate::health::assess::RunVerdict;
use crate::health::evidence::{
    ConfigStamp, Digest, Generation, GraphStamp, Head, HistoryObservation, HistoryStamp,
    InputFailure, InputStamp, MissingReason, ProducerOutcome, PublishRun, RepositoryObservation,
    RunId, StoredOutcome, UnsupportedReason, UtcDay, Validity, validate,
};
use crate::health::findings::BiomarkerKind;
use crate::health::probe::{
    self, DEFAULT_WINDOW_DAYS, analysis_version_digest, ingestion_version_digest,
    probe_graph_stamp, probe_owners, probe_repository, utc_day,
};
use crate::health::scoring::PERSISTENT_PRODUCERS;

/// Witness that the caller holds the per-workspace parse coordinator (in-process
/// serialisation) *and* the cross-process parse flock. Only a lock-holding
/// adapter can mint one — the full parse borrows the flock it already holds; the
/// demand adapter acquires both, then mints. The core takes `&HealthSession` on
/// every mutating entry point, so publishing without the exclusive scope does not
/// type-check. It is a design witness tied to a real held file lock, not a second
/// mutex (the contract: "no nested coordinator acquisition").
pub struct HealthSession<'lock> {
    _flock: &'lock std::fs::File,
}

impl<'lock> HealthSession<'lock> {
    /// Mint a session from an already-held parse flock. The caller MUST also hold
    /// the in-process parse coordinator for this workspace (the full parse runs
    /// inside `parse_workspace` under both; the demand adapter acquires both
    /// first). This never locks anything itself.
    pub(crate) fn from_held_flock(flock: &'lock std::fs::File) -> Self {
        Self { _flock: flock }
    }
}

/// Outcome of a refresh attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshResult {
    /// The retained run still reflects current inputs; nothing was rebuilt.
    Reused(RunId),
    /// A fresh run was published (may carry explicit partial outcomes).
    Published(RunId),
    /// The graph generation moved between staging and the publish transaction;
    /// nothing was published (no complete stamp over mixed inputs).
    InputsChanged,
}

/// Outcome of the demand-refresh *adapter* (Wave C): the acquiring adapter wraps
/// the core [`RefreshResult`] with the two states only it can produce — a
/// `Deferred` when a lock (coordinator or flock) was busy, and a `Failed` when the
/// refresh itself errored. In both non-`Refreshed` cases the retained run is still
/// readable; consumers surface it with explicit staleness rather than blocking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandOutcome {
    /// The core ran under both locks and reused or republished a run.
    Refreshed(RefreshResult),
    /// A lock was busy (coordinator wait timed out, flock held by another
    /// process, or a frozen index that cannot assert current filesystem health).
    Deferred(crate::health::evidence::DeferReason),
    /// The refresh errored under the lock; the prior run remains as evidence.
    Failed,
}

impl DemandOutcome {
    /// Stable validity token for this outcome, shared by the file-health evidence
    /// stamp (`file_health::attach_health_evidence`) and the review delta gate
    /// (`review::handle`, sutra/424 F3). Only `"current"` certifies that the live
    /// health tables reflect current inputs; every other token means a consumer
    /// must not present those numbers as verified-current — review reports the
    /// delta as incomparable rather than measuring on-demand debt against them.
    pub fn validity(&self) -> &'static str {
        use crate::health::evidence::DeferReason;
        match self {
            DemandOutcome::Refreshed(RefreshResult::Reused(_) | RefreshResult::Published(_)) => {
                "current"
            }
            // A race republished nothing; the retained run may not reflect current inputs.
            DemandOutcome::Refreshed(RefreshResult::InputsChanged) => "stale:inputs_changed",
            DemandOutcome::Deferred(DeferReason::LockBusy) => "deferred:lock_busy",
            DemandOutcome::Deferred(DeferReason::Frozen) => "deferred:frozen",
            DemandOutcome::Failed => "unavailable",
        }
    }

    /// The run this outcome vouches for and its [`Validity`], for
    /// [`crate::health::assess::PersistentEvidence::load`]. Only a reuse or a
    /// publication under the lock vouches for a run (and only for that run id);
    /// anything else leaves the retained evidence stale with the matching reason.
    pub fn verdict(&self) -> RunVerdict {
        match self {
            DemandOutcome::Refreshed(RefreshResult::Reused(id) | RefreshResult::Published(id)) => {
                RunVerdict {
                    run: Some(*id),
                    validity: Validity::Current,
                }
            }
            DemandOutcome::Refreshed(RefreshResult::InputsChanged) => {
                RunVerdict::stale(MissingReason::InputsChanged)
            }
            DemandOutcome::Deferred(reason) => RunVerdict::stale(MissingReason::Deferred(*reason)),
            DemandOutcome::Failed => RunVerdict::stale(MissingReason::RefreshFailed),
        }
    }
}

/// Validate the current run against freshly probed inputs without mutating
/// anything — for readers that did not just refresh (the snapshot writer). No
/// run is `Stale(LegacyUnknown)`.
pub fn current_run_validity(db: &Db, workspace_root: &Path, now_unix: i64) -> Result<RunVerdict> {
    let Some(run) = db.load_current_health_run()? else {
        return Ok(RunVerdict::stale(MissingReason::LegacyUnknown));
    };
    let observed = observe_inputs(
        db,
        workspace_root,
        utc_day(now_unix),
        window_days(workspace_root)?,
    )?;
    Ok(RunVerdict {
        run: Some(run.id),
        validity: validate(&run.inputs, &observed),
    })
}

/// The result of ingesting commit-file history, shared by the full parse (which
/// also needs `churn` for semantic anchors) and the demand refresh.
pub struct HistoryIngestion {
    /// The authoritative observation for the run's `InputStamp` — derived from
    /// what ingestion actually did, never re-inferred from `commit_file_count`.
    pub observation: HistoryObservation,
    /// Per-path commit churn (only populated when history loaded).
    pub churn: HashMap<String, u32>,
}

/// Whether a producer requires usable git history — the git-organizational and
/// churn biomarkers. When history is not `Loaded`, these produce no current
/// findings and their outcome reflects why (empty/absent/failed).
fn needs_history(kind: BiomarkerKind) -> bool {
    matches!(
        kind,
        BiomarkerKind::CoChangeScatter
            | BiomarkerKind::ChangeEntropy
            | BiomarkerKind::OwnershipRisk
            | BiomarkerKind::HiddenCoupling
            | BiomarkerKind::BlastRadiusChurn
    )
}

/// The trailing window length (days) for history selection: the workspace's
/// configured co-change window, defaulting to the contract's 90 days when unset.
/// A malformed `components.toml` is an error, not the default: the window is an
/// input-stamp axis, so falling back would silently record a different window.
pub fn window_days(workspace_root: &Path) -> Result<u32> {
    Ok(crate::components::load_config(workspace_root)?
        .cochange_window_days
        .unwrap_or(DEFAULT_WINDOW_DAYS))
}

/// Demand refresh (Wave C adapter → here): validate the retained run against
/// cheaply-probed inputs and reuse it if current; otherwise rebuild rollups and
/// history under the held lock and publish a fresh run. Ordinary symbol queries
/// must NOT call this — only health consumers do.
pub fn refresh(
    session: &HealthSession<'_>,
    db: &Db,
    workspace_root: &Path,
    now_unix: i64,
) -> Result<RefreshResult> {
    let window = window_days(workspace_root)?;
    let day = utc_day(now_unix);

    // (1) Cheap observe + validate. No mutation, no git log: probe the graph,
    // owners and (from current state) history, and compare against the retained
    // run. A `Current` verdict reuses it untouched.
    let observed = observe_inputs(db, workspace_root, day, window)?;
    if let Some(run) = db.load_current_health_run()?
        && matches!(validate(&run.inputs, &observed), Validity::Current)
    {
        return Ok(RefreshResult::Reused(run.id));
    }

    // (2) Stale (or no run): rebuild the inputs the producers consume. Rollups
    // feed BlastRadiusChurn; the demand path's incremental reparse resolved refs
    // but never recomputed rollups. History is re-ingested against the pinned
    // HEAD and the absolute day-quantized cutoff.
    rebuild_rollups(db)?;
    let graph = probe_graph_stamp(db)?;
    let ingestion = ingest_history(
        session,
        db,
        workspace_root,
        day,
        window,
        graph.generation,
        graph.indexed_paths,
    )?;

    // (3) Compute, stage and publish.
    publish_run(session, db, workspace_root, graph, ingestion.observation)
}

/// Acquire the cross-process parse flock (nonblocking) and run a demand
/// [`refresh`]. This is the reusable core of every *acquiring* health consumer:
/// the MCP demand adapter (which additionally holds the in-process coordinator
/// across a `spawn_blocking`), the review path (which already holds the
/// coordinator), and integration tests. On flock contention it returns
/// [`DemandOutcome::Deferred`] rather than blocking; the caller maps a refresh
/// `Err` to [`DemandOutcome::Failed`].
///
/// It never locks the in-process coordinator — that is the caller's concern (the
/// flock alone is the cross-process writer exclusion the contract requires here).
pub fn refresh_acquiring(
    config: &crate::config::Config,
    workspace_id: &str,
    db: &Db,
    workspace_root: &Path,
    now_unix: i64,
) -> Result<DemandOutcome> {
    let flock = match crate::pipeline::try_acquire_parse_flock(config, workspace_id)? {
        Some(f) => f,
        None => {
            return Ok(DemandOutcome::Deferred(
                crate::health::evidence::DeferReason::LockBusy,
            ));
        }
    };
    let session = HealthSession::from_held_flock(&flock);
    Ok(DemandOutcome::Refreshed(refresh(
        &session,
        db,
        workspace_root,
        now_unix,
    )?))
}

/// Rebuild file rollups (blast radius) over the current resolved graph. A no-op
/// on an empty index. Recomputes all files (no dirty hint): the demand path
/// cannot cheaply know which rollups a resolution change invalidated.
fn rebuild_rollups(db: &Db) -> Result<()> {
    let files = db.all_files()?;
    if files.is_empty() {
        return Ok(());
    }
    let gd = graph::GraphData::load(db)?;
    let adjacency = graph::build_file_adjacency(&files, &gd);
    graph::compute_rollups_with_adjacency(db, &files, &adjacency)?;
    Ok(())
}

/// Ingest commit-file history reachable from the pinned HEAD whose committer time
/// is `>= cutoff(day, window_days)`, writing `commit_files`. Shared by the full
/// parse and the demand refresh so both select history identically against the
/// absolute cutoff — never the relative `--since` the contract rejects as a
/// persistence contract. Returns the authoritative history observation for the
/// run stamp plus the churn map.
pub fn ingest_history(
    _session: &HealthSession<'_>,
    db: &Db,
    workspace_root: &Path,
    day: UtcDay,
    window_days: u32,
    generation: Generation,
    indexed_paths: Digest,
) -> Result<HistoryIngestion> {
    let cutoff = probe::history_cutoff(day, window_days);
    match probe_repository(workspace_root) {
        RepositoryObservation::ConfirmedAbsent { probe } => {
            // A confirmed non-repository has no history: clear any stale commit
            // data so the producers see nothing, and mark git unsupported.
            db.replace_commit_files(&[], &[])?;
            Ok(HistoryIngestion {
                observation: HistoryObservation::Unsupported {
                    absence_probe: probe,
                },
                churn: HashMap::new(),
            })
        }
        RepositoryObservation::Unknown(failure) => {
            // Indeterminate probe (git missing, access error, broken objects):
            // do NOT clear commit_files — a transient failure must not destroy
            // prior evidence. The run records the failure (git producers Missing),
            // while the coarse axis worst-cases as NoHistory.
            Ok(HistoryIngestion {
                observation: HistoryObservation::Unknown(failure),
                churn: HashMap::new(),
            })
        }
        RepositoryObservation::Present(repo_stamp) => {
            let stamp = HistoryStamp {
                repository: repo_stamp,
                day,
                window_days,
                indexed_paths,
                ingestion_version: ingestion_version_digest(),
                ingestion_generation: generation,
            };
            if matches!(stamp.repository.head, Head::Unborn) {
                // A present repo with no commits: a successful observation of no
                // usable history, not a failure.
                db.replace_commit_files(&[], &[])?;
                Ok(HistoryIngestion {
                    observation: HistoryObservation::Empty(stamp),
                    churn: HashMap::new(),
                })
            } else {
                ingest_present(db, workspace_root, cutoff, stamp)
            }
        }
    }
}

/// Ingest history for a present repository with a resolved HEAD.
fn ingest_present(
    db: &Db,
    workspace_root: &Path,
    cutoff: i64,
    stamp: HistoryStamp,
) -> Result<HistoryIngestion> {
    // Re-pin the HEAD sha (the observation only carries a digest of it). A race
    // to an unborn/broken state here is folded into empty/unknown; publication
    // re-probes repository identity/HEAD just before committing (`publish_run` →
    // `repository_moved`) and aborts with `InputsChanged` on a mismatch.
    let head_sha = match git::head_commit(workspace_root) {
        Ok(Some(sha)) => sha,
        Ok(None) => {
            db.replace_commit_files(&[], &[])?;
            return Ok(HistoryIngestion {
                observation: HistoryObservation::Empty(stamp),
                churn: HashMap::new(),
            });
        }
        Err(e) => {
            warn!("health: HEAD re-pin failed during history ingestion: {e}");
            return Ok(HistoryIngestion {
                observation: HistoryObservation::Unknown(InputFailure::ProbeFailed),
                churn: HashMap::new(),
            });
        }
    };

    // A shallow clone's object graph is truncated at the `.git/shallow`
    // boundary, so even a successful `git log` over the window cannot positively
    // establish that every qualifying commit is present: the range is
    // incomplete, not Loaded/Complete (health-evidence contract; sutra/427). The
    // git producers are worst-cased via `Unknown(HistoryIncomplete)`. Retain any
    // prior commit rows (do not clear) — an incomplete refresh must not destroy
    // evidence — and defer to `Unknown(ProbeFailed)` if the shallow probe itself
    // is indeterminate.
    match git::is_shallow_repository(workspace_root) {
        Ok(true) => {
            return Ok(HistoryIngestion {
                observation: HistoryObservation::Unknown(InputFailure::HistoryIncomplete),
                churn: HashMap::new(),
            });
        }
        Ok(false) => {}
        Err(e) => {
            warn!("health: shallow-repository probe failed during history ingestion: {e}");
            return Ok(HistoryIngestion {
                observation: HistoryObservation::Unknown(InputFailure::ProbeFailed),
                churn: HashMap::new(),
            });
        }
    }

    match git::git_commit_files_since(workspace_root, &head_sha, cutoff) {
        Ok(commit_files) if !commit_files.is_empty() => {
            // Loaded means an *indexed* file has in-window history: a window
            // touching only unindexed paths observed nothing for the files the
            // producers score, so it is Empty, not Loaded (sutra/423).
            if write_commit_files(db, &commit_files)? == 0 {
                db.replace_commit_files(&[], &[])?;
                return Ok(HistoryIngestion {
                    observation: HistoryObservation::Empty(stamp),
                    churn: HashMap::new(),
                });
            }
            Ok(HistoryIngestion {
                observation: HistoryObservation::Loaded(stamp),
                churn: git::churn_from_commit_files(&commit_files),
            })
        }
        Ok(_) => {
            // The window holds no commits: a successful observation of no usable
            // history for this repo.
            db.replace_commit_files(&[], &[])?;
            Ok(HistoryIngestion {
                observation: HistoryObservation::Empty(stamp),
                churn: HashMap::new(),
            })
        }
        Err(e) => {
            warn!("health: git commit-file history ingestion failed: {e}");
            // Retain prior evidence; the range could not be established, so this
            // is partial (Unknown), not a clean empty observation.
            Ok(HistoryIngestion {
                observation: HistoryObservation::Unknown(InputFailure::IngestionFailed),
                churn: HashMap::new(),
            })
        }
    }
}

/// Persist ingested commit-file rows: one `commits` row per distinct hash and one
/// `commit_files` edge per (hash, indexed file). Paths not indexed are dropped.
fn write_commit_files(db: &Db, commit_files: &[git::CommitFile]) -> Result<usize> {
    let files = db.all_files()?;
    let path_to_id: HashMap<&str, i64> = files.iter().map(|f| (&*f.path, f.id)).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut commit_rows = Vec::new();
    for cf in commit_files {
        if seen.insert(cf.hash.as_str()) {
            commit_rows.push(CommitRow {
                hash: cf.hash.to_string(),
                committed_at: cf.timestamp,
                author: cf.author.to_string(),
            });
        }
    }
    let pairs: Vec<(String, i64)> = commit_files
        .iter()
        .filter_map(|cf| {
            path_to_id
                .get(cf.path.as_str())
                .map(|&id| (cf.hash.to_string(), id))
        })
        .collect();
    db.replace_commit_files(&commit_rows, &pairs)
}

/// Publish a run from the current (already-rebuilt) graph, history and owners
/// state. Called by the full parse directly (it holds the flock and has already
/// rebuilt rollups + ingested history) and by [`refresh`] after its rebuild. The
/// `history` observation is the authoritative one from ingestion, never
/// re-derived. Also replaces the live `health_findings` table (diagnostic
/// listings; the assigned row ids are what the run retains). Scoring reads the
/// published run, never the live table (sutra/416).
pub fn publish_run(
    _session: &HealthSession<'_>,
    db: &Db,
    workspace_root: &Path,
    graph: GraphStamp,
    history: HistoryObservation,
) -> Result<RefreshResult> {
    let owners = probe_owners(workspace_root);
    let history_loaded = matches!(history, HistoryObservation::Loaded(_));

    // Compute findings through the shared producers. When history is not loaded,
    // the git-organizational producers have no current data: drop their findings
    // so the live table and the run agree, and stage them Missing/Unsupported
    // below instead of publishing stale retained findings as current.
    let mut findings = crate::health::compute_all_health_findings(db, workspace_root)?;
    if !history_loaded {
        findings.retain(|f| !needs_history(f.biomarker_kind));
    }
    db.replace_health_findings(&findings)?;

    // Re-read the rows to capture the assigned ids, then label them for retention.
    let rows = db.get_health_findings(None, None)?;
    let files = db.all_files()?;
    let path_by_id: HashMap<i64, String> =
        files.iter().map(|f| (f.id, f.path.to_string())).collect();

    // Stage outcomes while we still borrow the rows; then consume the rows into
    // retained findings (moving each row, no clone).
    // Per-file history granularity (contract "repository/clock policy"): a
    // workspace-level Loaded does not prove history for every file. The
    // `commit_files` rows ingestion just wrote under this lock are exactly the
    // indexed files with in-window commits; a file outside a producer's usable
    // subset (its commit-width filter applied) stages Missing(NoHistory).
    let history_files: HashMap<BiomarkerKind, HashSet<i64>> = if history_loaded {
        PERSISTENT_PRODUCERS
            .into_iter()
            .filter(|&kind| needs_history(kind))
            .map(|kind| {
                let width = crate::health::git_metrics::max_observed_commit_width(kind);
                Ok((kind, db.history_file_ids(width)?))
            })
            .collect::<Result<_>>()?
    } else {
        HashMap::new()
    };
    let outcomes = stage_outcomes(
        &files,
        &rows,
        &path_by_id,
        &history,
        &history_files,
        &owners.stamp,
    );
    let stored_findings = crate::health::assess::label_findings(db, rows, &path_by_id)?;

    // Recheck before commit (contract "Publication and consumers", sutra/425):
    // re-probe the repository just before publishing and abort if it moved since
    // the caller observed the `history` we are about to stamp. A HEAD move / repo
    // state change mid-refresh does NOT bump the graph generation (which advances
    // only on file insert/delete + resolution), so `publish_health_run`'s
    // generation guard alone would let findings computed against the pre-move
    // history publish as a complete, current stamp over mixed inputs. The stale
    // run self-heals on the next refresh (`observe_history` sees the new HEAD).
    if repository_moved(&history, workspace_root) {
        return Ok(RefreshResult::InputsChanged);
    }

    let epoch = graph.epoch;
    let generation = graph.generation;
    let inputs = InputStamp {
        graph,
        history,
        owners: owners.stamp,
        // On the success path rollups are current at the graph generation (415
        // stamp-construction invariant: Ok(generation), never a lagging Ok).
        rollups: Ok(generation),
        analysis_version: analysis_version_digest(),
    };

    let run = PublishRun {
        index_epoch: epoch,
        graph_generation: generation,
        inputs,
        outcomes,
        findings: stored_findings,
    };

    match db.publish_health_run(generation, &run)? {
        Some(id) => Ok(RefreshResult::Published(id)),
        None => Ok(RefreshResult::InputsChanged),
    }
}

/// Re-probe repository identity / HEAD / history boundaries and report whether
/// they diverge from what the `history` observation (computed earlier by the
/// caller during ingestion) was derived against. `true` means the repository
/// moved under us since ingestion — a HEAD advance, an identity/boundary change,
/// or the repo appearing/vanishing — and publication must abort rather than
/// stamp a complete run over mixed inputs (see the call site in [`publish_run`]).
///
/// Only the repository fingerprint is compared, not the full history observation:
/// the other stamp axes (day, window, indexed paths, ingestion generation) are
/// fixed by the same `graph`/clock the caller already pinned, and the born-HEAD
/// `Loaded`/`Empty` distinction is decided from `commit_files` written under this
/// same held lock — so the repository probe is the only axis an external actor
/// can change between ingestion and here.
fn repository_moved(history: &HistoryObservation, workspace_root: &Path) -> bool {
    let observed = probe_repository(workspace_root);
    match history {
        // Both born-history states pinned a concrete repository stamp: require an
        // identical present repository now. Anything else (moved HEAD, changed
        // identity/boundaries, now absent, now an indeterminate probe) is a move.
        HistoryObservation::Loaded(stamp) | HistoryObservation::Empty(stamp) => {
            !matches!(&observed, RepositoryObservation::Present(rs) if *rs == stamp.repository)
        }
        // A confirmed non-repository must still be confirmed absent with the same
        // probe fingerprint; a repo appearing (or a differing probe) is a move.
        HistoryObservation::Unsupported { absence_probe } => {
            !matches!(&observed, RepositoryObservation::ConfirmedAbsent { probe } if probe == absence_probe)
        }
        // The observation already records a failed probe: the run stamps the git
        // producers Missing(Failed) regardless of current repo state, so there is
        // no mixed-input hazard to recheck.
        HistoryObservation::Unknown(_) => false,
    }
}

/// Stage one explicit [`StoredOutcome`] per (file, producer): `Complete { n }`
/// (with `n` the retained findings for that file+producer), `Missing(reason)`, or
/// `Unsupported`. A successful empty producer is `Complete { 0 }`, explicitly
/// distinct from a missing one. `history_files` holds, per git producer, the ids
/// of files with in-window commits that producer consumes; under `Loaded`
/// history a file outside its producer's set has no usable history
/// observations, so that producer is `Missing(NoHistory)`.
fn stage_outcomes(
    files: &[crate::db::FileRow],
    rows: &[HealthFindingRow],
    path_by_id: &HashMap<i64, String>,
    history: &HistoryObservation,
    history_files: &HashMap<BiomarkerKind, HashSet<i64>>,
    owners: &std::result::Result<ConfigStamp, InputFailure>,
) -> Vec<StoredOutcome> {
    // Per (file_id, producer) finding counts from the retained rows.
    let mut counts: HashMap<(i64, BiomarkerKind), usize> = HashMap::new();
    for row in rows {
        if let Some(kind) = BiomarkerKind::parse(&row.biomarker_kind) {
            *counts.entry((row.file_id, kind)).or_default() += 1;
        }
    }

    let mut outcomes = Vec::with_capacity(files.len() * PERSISTENT_PRODUCERS.len());
    for file in files {
        let Some(path) = path_by_id.get(&file.id) else {
            continue;
        };
        for kind in PERSISTENT_PRODUCERS {
            let count = counts.get(&(file.id, kind)).copied().unwrap_or(0);
            let has_history = history_files
                .get(&kind)
                .is_some_and(|ids| ids.contains(&file.id));
            outcomes.push(StoredOutcome {
                file_path: path.to_string(),
                producer: kind,
                outcome: producer_outcome(kind, history, has_history, owners, count),
            });
        }
    }
    outcomes
}

/// The outcome for one producer given the observed history (workspace-level,
/// plus whether this file has in-window commits) and owners config.
fn producer_outcome(
    kind: BiomarkerKind,
    history: &HistoryObservation,
    has_history: bool,
    owners: &std::result::Result<ConfigStamp, InputFailure>,
    count: usize,
) -> ProducerOutcome {
    match kind {
        // Structural producers depend only on extraction/resolution, current here.
        BiomarkerKind::NestedComplexity
        | BiomarkerKind::ImportCycle
        | BiomarkerKind::DeadCodeRatio => ProducerOutcome::Complete {
            finding_count: count,
        },
        // No coverage ingestion exists anywhere in the repo.
        BiomarkerKind::CoverageGradient => {
            ProducerOutcome::Unsupported(UnsupportedReason::NoCoverageIngestion)
        }
        // Ownership additionally needs a successfully-parsed owners config: an
        // Err stamp is a failure the producer must not paper over with a default.
        BiomarkerKind::OwnershipRisk => match git_outcome(history, has_history, count) {
            ProducerOutcome::Complete { .. } => match owners {
                Err(failure) => ProducerOutcome::Missing(MissingReason::Failed(*failure)),
                Ok(_) => ProducerOutcome::Complete {
                    finding_count: count,
                },
            },
            other => other,
        },
        // The remaining git-organizational / churn producers.
        _ => git_outcome(history, has_history, count),
    }
}

/// Map a history observation to a git-producer outcome for one file. Workspace
/// `Loaded` is necessary but not sufficient: a file with no in-window commits has
/// no usable history observations, which is `Missing(NoHistory)` — never a
/// measured-clean `Complete { 0 }` (sutra/423).
fn git_outcome(history: &HistoryObservation, has_history: bool, count: usize) -> ProducerOutcome {
    match history {
        HistoryObservation::Loaded(_) if has_history => ProducerOutcome::Complete {
            finding_count: count,
        },
        HistoryObservation::Loaded(_) => ProducerOutcome::Missing(MissingReason::NoHistory),
        HistoryObservation::Empty(_) => ProducerOutcome::Missing(MissingReason::NoHistory),
        HistoryObservation::Unsupported { .. } => {
            ProducerOutcome::Unsupported(UnsupportedReason::ConfirmedNonRepository)
        }
        HistoryObservation::Unknown(failure) => {
            ProducerOutcome::Missing(MissingReason::Failed(*failure))
        }
    }
}

/// Build the cheap "observed now" input stamp used only to decide whether the
/// retained run may be reused. No mutation, no `git log`: history is inferred from
/// the current `commit_files` population (truthful because ingestion last wrote
/// it under this same lock). Any real input change (edit → generation bump, HEAD
/// move, crossed midnight, changed window/paths/owners) diverges from the
/// recorded stamp and forces the rebuild.
fn observe_inputs(
    db: &Db,
    workspace_root: &Path,
    day: UtcDay,
    window_days: u32,
) -> Result<InputStamp> {
    let graph = probe_graph_stamp(db)?;
    let owners = probe_owners(workspace_root).stamp;
    let history = observe_history(db, workspace_root, day, window_days, &graph)?;
    let generation = graph.generation;
    Ok(InputStamp {
        graph,
        history,
        owners,
        rollups: Ok(generation),
        analysis_version: analysis_version_digest(),
    })
}

/// Cheaply observe the current history without ingesting: infer loaded/empty from
/// `commit_file_count`. Used only for the reuse decision.
fn observe_history(
    db: &Db,
    workspace_root: &Path,
    day: UtcDay,
    window_days: u32,
    graph: &GraphStamp,
) -> Result<HistoryObservation> {
    match probe_repository(workspace_root) {
        RepositoryObservation::ConfirmedAbsent { probe } => Ok(HistoryObservation::Unsupported {
            absence_probe: probe,
        }),
        RepositoryObservation::Unknown(failure) => Ok(HistoryObservation::Unknown(failure)),
        RepositoryObservation::Present(repo_stamp) => {
            let stamp = HistoryStamp {
                repository: repo_stamp,
                day,
                window_days,
                indexed_paths: graph.indexed_paths,
                ingestion_version: ingestion_version_digest(),
                ingestion_generation: graph.generation,
            };
            if matches!(stamp.repository.head, Head::Unborn) {
                Ok(HistoryObservation::Empty(stamp))
            } else if git::is_shallow_repository(workspace_root)? {
                // Mirror `ingest_present`: a shallow clone's window is incomplete,
                // never Loaded. Classifying it identically here keeps the cheap
                // reuse probe honest — the stored observation is
                // `Unknown(HistoryIncomplete)`, and `history_validity` refreshes
                // rather than reuse a history it cannot confirm complete (sutra/427).
                Ok(HistoryObservation::Unknown(InputFailure::HistoryIncomplete))
            } else if db.commit_file_count()? > 0 {
                Ok(HistoryObservation::Loaded(stamp))
            } else {
                Ok(HistoryObservation::Empty(stamp))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::evidence::{DeferReason, RunId};

    // sutra/432: the window is an input-stamp axis, so a malformed config must
    // fail loudly instead of silently stamping the default window.
    #[test]
    fn window_days_default_configured_and_malformed() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            window_days(dir.path()).expect("absent config"),
            DEFAULT_WINDOW_DAYS
        );

        let sutra_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&sutra_dir).expect("create .sutra");
        let cfg = sutra_dir.join("components.toml");
        std::fs::write(&cfg, "cochange_window_days = 30\n").expect("write config");
        assert_eq!(window_days(dir.path()).expect("valid config"), 30);

        std::fs::write(&cfg, "cochange_window_days = \"thirty\"\n").expect("write config");
        assert!(window_days(dir.path()).is_err());
    }

    // The validity token is the single contract seam shared by the file-health
    // evidence stamp and the review delta gate (sutra/424 F3); only "current"
    // may certify the live tables. Lock the mapping so neither consumer drifts.
    #[test]
    fn demand_outcome_validity_tokens() {
        assert_eq!(
            DemandOutcome::Refreshed(RefreshResult::Reused(RunId(1))).validity(),
            "current"
        );
        assert_eq!(
            DemandOutcome::Refreshed(RefreshResult::Published(RunId(2))).validity(),
            "current"
        );
        assert_eq!(
            DemandOutcome::Refreshed(RefreshResult::InputsChanged).validity(),
            "stale:inputs_changed"
        );
        assert_eq!(
            DemandOutcome::Deferred(DeferReason::LockBusy).validity(),
            "deferred:lock_busy"
        );
        assert_eq!(
            DemandOutcome::Deferred(DeferReason::Frozen).validity(),
            "deferred:frozen"
        );
        assert_eq!(DemandOutcome::Failed.validity(), "unavailable");
    }

    // --- repository_moved: the pre-publication recheck (sutra/425) ---

    fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
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

    fn git_init_commit(root: &Path) {
        git(root, &["init", "-q"]);
        git(root, &["config", "user.email", "t@example.com"]);
        git(root, &["config", "user.name", "T"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "-A"]);
        // --no-verify skips any inherited pre-commit hook; gpgsign off keeps the
        // commit hermetic on hosts with global signing enabled.
        git(
            root,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--no-verify",
                "-m",
                "one",
            ],
        );
    }

    /// A `Loaded` history observation pinned to the repository's current state.
    /// Only `repository` is inspected by `repository_moved`; the remaining stamp
    /// axes are filler.
    fn loaded_history(root: &Path) -> HistoryObservation {
        match probe_repository(root) {
            RepositoryObservation::Present(repository) => {
                HistoryObservation::Loaded(HistoryStamp {
                    repository,
                    day: UtcDay(0),
                    window_days: 90,
                    indexed_paths: Digest::of(b"paths"),
                    ingestion_version: Digest::of(b"iv"),
                    ingestion_generation: Generation(0),
                })
            }
            other => panic!("expected a present repository, got {other:?}"),
        }
    }

    #[test]
    fn repository_moved_is_false_when_head_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        git_init_commit(dir.path());
        let history = loaded_history(dir.path());
        assert!(
            !repository_moved(&history, dir.path()),
            "an unchanged HEAD must not read as moved"
        );
    }

    #[test]
    fn repository_moved_detects_a_head_advance() {
        let dir = tempfile::tempdir().unwrap();
        git_init_commit(dir.path());
        // History pinned to the first commit.
        let history = loaded_history(dir.path());

        // Advance HEAD: a mid-refresh commit does not bump the graph generation,
        // so only this repository recheck catches it.
        std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(
            dir.path(),
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--no-verify",
                "-m",
                "two",
            ],
        );

        assert!(
            repository_moved(&history, dir.path()),
            "a HEAD advance since ingestion must abort publication"
        );
    }

    #[test]
    fn repository_moved_ignores_a_failed_history_probe() {
        // An `Unknown` observation already stamps git producers Missing(Failed);
        // there is no established history to invalidate, so the recheck is a no-op
        // regardless of the current path state.
        let dir = tempfile::tempdir().unwrap();
        let history = HistoryObservation::Unknown(InputFailure::IngestionFailed);
        assert!(!repository_moved(&history, dir.path()));
    }
}
