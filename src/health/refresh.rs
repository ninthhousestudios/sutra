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
use crate::health::evidence::{
    ConfigStamp, Digest, Generation, GraphStamp, Head, HistoryObservation, HistoryStamp,
    InputFailure, InputStamp, MissingReason, ProducerOutcome, PublishRun, RepositoryObservation,
    RunId, StoredFinding, StoredOutcome, UnsupportedReason, UtcDay, Validity, validate,
};
use crate::health::findings::BiomarkerKind;
use crate::health::probe::{
    self, DEFAULT_WINDOW_DAYS, analysis_version_digest, ingestion_version_digest,
    probe_graph_stamp, probe_owners, probe_repository, utc_day,
};
use crate::health::scoring::GitAvailability;

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

/// The result of ingesting commit-file history, shared by the full parse (which
/// also needs `churn` for semantic anchors and `availability` for the legacy
/// scoring axis) and the demand refresh.
pub struct HistoryIngestion {
    /// The authoritative observation for the run's `InputStamp` — derived from
    /// what ingestion actually did, never re-inferred from `commit_file_count`.
    pub observation: HistoryObservation,
    /// Coarse git-availability axis persisted for the legacy `score_workspace`
    /// path (`index_meta.git_availability`).
    pub availability: GitAvailability,
    /// Per-path commit churn (only populated when history loaded).
    pub churn: HashMap<String, u32>,
}

/// File-scored parse-time producers a run stages an explicit per-file outcome for
/// (exactly the biomarkers `BiomarkerKind::file_scoring_support` classifies as
/// `Some`). On-demand and component-scoped biomarkers are scored in their own
/// paths and are not part of the persisted run.
const RUN_PRODUCERS: [BiomarkerKind; 9] = [
    BiomarkerKind::NestedComplexity,
    BiomarkerKind::CoChangeScatter,
    BiomarkerKind::ChangeEntropy,
    BiomarkerKind::OwnershipRisk,
    BiomarkerKind::HiddenCoupling,
    BiomarkerKind::BlastRadiusChurn,
    BiomarkerKind::DeadCodeRatio,
    BiomarkerKind::ImportCycle,
    BiomarkerKind::CoverageGradient,
];

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
/// configured co-change window, defaulting to the contract's 90 days.
pub fn window_days(workspace_root: &Path) -> u32 {
    crate::components::load_config(workspace_root)
        .ok()
        .and_then(|c| c.cochange_window_days)
        .unwrap_or(DEFAULT_WINDOW_DAYS)
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
    let window = window_days(workspace_root);
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
    db.set_git_availability(ingestion.availability.as_str())?;

    // (3) Compute, stage and publish.
    publish_run(session, db, workspace_root, graph, ingestion.observation)
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
    graph::compute_rollups_with_adjacency(db, &files, &adjacency, None)?;
    Ok(())
}

/// Ingest commit-file history reachable from the pinned HEAD whose committer time
/// is `>= cutoff(day, window_days)`, writing `commit_files`. Shared by the full
/// parse and the demand refresh so both select history identically against the
/// absolute cutoff — never the relative `--since` the contract rejects as a
/// persistence contract. Returns the authoritative history observation for the
/// run stamp plus the coarse availability axis and the churn map.
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
                availability: GitAvailability::NotARepo,
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
                availability: GitAvailability::NoHistory,
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
                    availability: GitAvailability::NoHistory,
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
    // re-checks repository identity under the transaction regardless.
    let head_sha = match git::head_commit(workspace_root) {
        Ok(Some(sha)) => sha,
        Ok(None) => {
            db.replace_commit_files(&[], &[])?;
            return Ok(HistoryIngestion {
                observation: HistoryObservation::Empty(stamp),
                availability: GitAvailability::NoHistory,
                churn: HashMap::new(),
            });
        }
        Err(e) => {
            warn!("health: HEAD re-pin failed during history ingestion: {e}");
            return Ok(HistoryIngestion {
                observation: HistoryObservation::Unknown(InputFailure::ProbeFailed),
                availability: GitAvailability::NoHistory,
                churn: HashMap::new(),
            });
        }
    };

    match git::git_commit_files_since(workspace_root, &head_sha, cutoff) {
        Ok(commit_files) if !commit_files.is_empty() => {
            let churn = git::churn_from_commit_files(&commit_files);
            write_commit_files(db, &commit_files)?;
            Ok(HistoryIngestion {
                observation: HistoryObservation::Loaded(stamp),
                availability: GitAvailability::Available,
                churn,
            })
        }
        Ok(_) => {
            // The window holds no commits: a successful observation of no usable
            // history for this repo.
            db.replace_commit_files(&[], &[])?;
            Ok(HistoryIngestion {
                observation: HistoryObservation::Empty(stamp),
                availability: GitAvailability::NoHistory,
                churn: HashMap::new(),
            })
        }
        Err(e) => {
            warn!("health: git commit-file history ingestion failed: {e}");
            // Retain prior evidence; the range could not be established, so this
            // is partial (Unknown), not a clean empty observation.
            Ok(HistoryIngestion {
                observation: HistoryObservation::Unknown(InputFailure::IngestionFailed),
                availability: GitAvailability::NoHistory,
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
/// re-derived. Also refreshes the live `health_findings`/`health_coverage`
/// tables the legacy scoring path reads, so full parse and demand agree.
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
    let symbol_ids: Vec<i64> = rows.iter().filter_map(|r| r.symbol_id).collect();
    let labels = db.symbol_labels(&symbol_ids)?;

    // Stage outcomes while we still borrow the rows; then consume the rows into
    // retained findings (moving each row, no clone).
    let outcomes = stage_outcomes(&files, &rows, &path_by_id, &history, &owners.stamp);
    let stored_findings: Vec<StoredFinding> = rows
        .into_iter()
        .map(|row| StoredFinding {
            file_path: path_by_id
                .get(&row.file_id)
                .map(|p| p.to_string())
                .unwrap_or_else(|| "?".to_string()),
            symbol_label: row
                .symbol_id
                .and_then(|sid| labels.get(&sid).map(|s| s.to_string())),
            finding: row,
        })
        .collect();

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

/// Stage one explicit [`StoredOutcome`] per (file, producer): `Complete { n }`
/// (with `n` the retained findings for that file+producer), `Missing(reason)`, or
/// `Unsupported`. A successful empty producer is `Complete { 0 }`, explicitly
/// distinct from a missing one.
fn stage_outcomes(
    files: &[crate::db::FileRow],
    rows: &[HealthFindingRow],
    path_by_id: &HashMap<i64, String>,
    history: &HistoryObservation,
    owners: &std::result::Result<ConfigStamp, InputFailure>,
) -> Vec<StoredOutcome> {
    // Per (file_id, producer) finding counts from the retained rows.
    let mut counts: HashMap<(i64, BiomarkerKind), usize> = HashMap::new();
    for row in rows {
        if let Some(kind) = BiomarkerKind::parse(&row.biomarker_kind) {
            *counts.entry((row.file_id, kind)).or_default() += 1;
        }
    }

    let mut outcomes = Vec::with_capacity(files.len() * RUN_PRODUCERS.len());
    for file in files {
        let Some(path) = path_by_id.get(&file.id) else {
            continue;
        };
        for kind in RUN_PRODUCERS {
            let count = counts.get(&(file.id, kind)).copied().unwrap_or(0);
            outcomes.push(StoredOutcome {
                file_path: path.to_string(),
                producer: kind,
                outcome: producer_outcome(kind, history, owners, count),
            });
        }
    }
    outcomes
}

/// The outcome for one producer given the observed history and owners config.
fn producer_outcome(
    kind: BiomarkerKind,
    history: &HistoryObservation,
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
        BiomarkerKind::OwnershipRisk => match git_outcome(history, count) {
            ProducerOutcome::Complete { .. } => match owners {
                Err(failure) => ProducerOutcome::Missing(MissingReason::Failed(*failure)),
                Ok(_) => ProducerOutcome::Complete {
                    finding_count: count,
                },
            },
            other => other,
        },
        // The remaining git-organizational / churn producers.
        _ => git_outcome(history, count),
    }
}

/// Map a history observation to a git-producer outcome.
fn git_outcome(history: &HistoryObservation, count: usize) -> ProducerOutcome {
    match history {
        HistoryObservation::Loaded(_) => ProducerOutcome::Complete {
            finding_count: count,
        },
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
            } else if db.commit_file_count()? > 0 {
                Ok(HistoryObservation::Loaded(stamp))
            } else {
                Ok(HistoryObservation::Empty(stamp))
            }
        }
    }
}
