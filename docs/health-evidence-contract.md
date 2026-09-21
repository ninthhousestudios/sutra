# Health evidence contract

Status: proposed for human review, sutra/412. No production implementation.
This contract replaces the two-axis proposal in sutra/411 and the snapshot
mirroring rationale in sutra/409. Session-start reparse remains enabled.

## Identity and validity

Keep `files.id` stable for a path during content replacement. Replace extraction
children, detach and re-resolve inbound references, and audit every child table
in sutra/413. An actual removal remains a removal; a rename is removal/addition
unless separately proven. Never attach old history to a new path by recycled ID.
Snapshots identify files by workspace-relative path and index epoch, not live
symbol IDs. Retained findings include their original path/symbol label and input
stamp; deleting extraction must not rewrite their provenance.
Immutable evidence storage must not cascade through live file/symbol foreign
keys. Numeric IDs in retained rows are diagnostic only, never re-resolved against
replacement extraction; use the captured path and symbol label for display.

Use a persisted index epoch plus a conservative workspace graph generation.
Increment generation in the same transaction as every extraction, resolution,
import-edge, indexed-population or relevant graph mutation. Audit writers before
reusing `data_generation`: its existence alone does not prove complete coverage.
Parser identity and successful extraction coverage are separate prerequisites.
An incomplete walk, retained old-extractor row or unresolved refresh failure
cannot mint a complete graph input. Health publication has its own sequence;
it never calls `set_derived_complete`.

Initially recompute all persistent file producers when any required input moves.
Per-producer validity is represented for honest partial results, not a mandate
for selective recomputation. Successful empty output is recorded explicitly for
every applicable producer and file; absence of findings proves nothing alone.

| Producer/product | Required inputs beyond file identity |
|---|---|
| NestedComplexity | Successful extraction, parser stamp, source fingerprint |
| ImportCycle | Complete resolved workspace import graph |
| DeadCodeRatio | Complete workspace symbols/references and resolution |
| CoChangeScatter, ChangeEntropy | Successful mapped history, repository identity, pinned HEAD, ingestion version, window, indexed paths |
| OwnershipRisk | Same history plus successfully parsed owners configuration |
| HiddenCoupling | Same history **and** current static graph edges |
| BlastRadiusChurn | Same history **and** rollups computed for current graph |
| ComponentInstability/aggregate | Current component membership/configuration, import graph, current file scores and NLOC |
| FunctionHotspot, CodeAgeVolatility | Per-review successful blame, pinned repository/base, source/extraction identity, analysis version |
| HrrShapeChange | Explicit review base, current source, parser/encoder/config identity; successful shape analysis |
| CoverageGradient | Unsupported until an ingestion contract exists |

All rows also depend on producer-version identity. Scored observations record
scoring version, included biomarker set, applicability, category caps and waiver
policy digest. Health versions cover the implementation/configuration that
changes findings; parser stamp alone does not. Store explicit version digests,
including resolver/rollup versions, rather than guessing freshness from HEAD or
`content_hash`. Owners absence has a distinct default digest; malformed or
unreadable owners data is a failure, not an empty/default configuration.

## Repository and clock policy

Choose a **UTC-day-quantized trailing window**. At request time `t`, use
`cutoff = floor(t / 86400) * 86400 - window_days * 86400`. Ingest commits reachable
from a pinned HEAD whose **committer timestamp is >= cutoff**; there is no upper
timestamp filter. Future-dated commits reachable from HEAD are included. This
is deliberately day-granular, not a promise of an exact trailing 90×24 hours.
Expiry is the next UTC midnight, exclusive, even if HEAD is unchanged or the
selected set happens to remain identical. A clock moving to a different bucket
also invalidates. A changed window length invalidates immediately. Default stays
90 days. Entropy decay remains relative to newest included commit, as today;
changing that reference is an analysis-version change.

Reject relative `git log --since="N days ago"` as the persistence contract:
its moving cutoff cannot be reproduced from a stored HEAD. Select against an
absolute cutoff and committer timestamps, without a traversal early-stop that
misses qualifying commits behind nonmonotonic dates. Record repository/worktree
identity, HEAD (including unborn state), shallow/replacement boundary fingerprint,
ingestion schema/version, indexed-path digest, bucket and successful ingestion
generation. Check repository identity/HEAD/boundaries again before publication.
History inaccessible or incomplete because of shallow/missing objects is partial,
unless completeness of the requested range is positively established.

Repository states are Present, ConfirmedAbsent, and Unknown. Only confirmed
absence makes git biomarkers unsupported. An arbitrary nonzero exit, spawn
failure, access error, malformed output or failed HEAD probe is Unknown. Retain
prior evidence for display with its original stamp; do not infer currentness from
nonzero `commit_file_count`.

A successful empty repository/window/mapped history is a successful observation
of **no usable history**, not a clean git-health measurement. Git producers are
missing with `NoHistory`; structural producers can still be current. Likewise a
file with no usable history observations has missing git analysis even if another
file has commits. Nonempty successfully analyzed file history may yield zero
findings with current coverage. Unknown and NoHistory are distinct output reasons.

## Publication and consumers

Acquire the existing per-workspace parse coordinator once, with its bounded
two-second wait. The outer adapter owns the guard and `ParseMark` across the
blocking worker, including cancellation. A private `HealthSession` borrows that
exclusive scope; the refresh core never locks the coordinator recursively.
Full parse and DD-backed review call the already-locked core. Other health readers
use an acquiring adapter. Cross-process database writer exclusion remains owned
by `acquire_parse_flock` (`pipeline.rs`), shared by full and incremental parse;
a process mutex is not a cross-process lock. Factor its ownership so full parse
passes its already-held file lock into health, while standalone health acquires
it once after the coordinator. Use a bounded nonblocking flock attempt for demand
refresh; file-lock contention also returns LockBusy. Never reacquire a flock on
a second descriptor within the already-locked core.

Under the lock, refresh extraction if necessary, then re-probe inputs. Rebuild
required resolution/rollups and history through shared existing producers. Stage
findings, per-file/per-producer outcomes and prerequisite stamps. In one SQLite
transaction verify the input generation and publish one immutable health run plus
its current pointer. Readers load pointer, outcomes and findings in one read
transaction. Recheck filesystem/config/repository probes before committing; a
race aborts publication and returns InputsChanged, never a complete stamp for
mixed inputs. Freshness is an as-of observation, not a guarantee against edits
after the final probe. Invalidated old runs remain diagnostic evidence only.

Producer-local failure may publish a coherent partial run with explicit failures
and successful independent producers. Input-race/storage failure publishes
nothing. Lock timeout/frozen index reports deferred with retained provenance;
there is no optimistic fallback. A frozen index can serve historical evidence,
but cannot assert current filesystem health without validation.

The required consumer wiring is:

| Consumer | Contract |
|---|---|
| File health and workspace health summaries | Demand-refresh file evidence, read coherent run, expose validity and partiality |
| Review | Capture baseline ID **before** `tool_context` can full-parse; refresh within its DD lock; temporal comparison and on-demand attribution are separate |
| Full parse/snapshot writer | Call shared already-locked refresh; checkpoint only after publication, storing validity and score basis atomically |
| Trend comparison/history | Read immutable checkpoints, preserve both sides' completeness; never refresh old evidence or relabel it |
| File-health component scores and snapshot component aggregates | Use membership/instability only when their prerequisite stamp is current; otherwise component score is unavailable/partial |
| Shared `score_workspace`/`score_file` entry points | Consume validated observations, not a raw coverage bool; no finding-count escape hatch |

Verified production entry points are `tools/file_health.rs`, `tools/review.rs`,
`tools/trend.rs` and `pipeline.rs::compute_snapshot_health`. `sutra_health`, CLI
`cmd_health` and REST `/health` are operational diagnostics, not biomarker scoring.
`FreshnessAnnotator` annotates content freshness, not health evidence; it must not
be treated as proof of health validity. Ordinary symbol queries do not rebuild
health. Review must stop discarding health failures through `.ok()` and apply the
same waiver policy as file health.

Health-only refresh rebuilds file rollups but does not run HRR, conventions or
component clustering, nor claim those products current. Component membership can
depend on history/configuration as well as graph; stale membership is explicit
unavailability, not a penalty computed using old membership. Full parse repairs
component products. Do not mark the whole derived tier complete from health.

Health refresh never records a checkpoint. Existing explicit/full parse remains
a checkpoint event. Review defaults to the latest checkpoint at request entry,
or accepts an explicit baseline; a missing baseline remains missing. Pin its ID
and retain it through the request, even if parser healing makes a newer snapshot.
NoChanges snapshot copy-forward is allowed only for unchanged health inputs and
score basis, not merely unchanged source bytes.

## Comparison and scoring

Temporal comparison uses persistent evidence only. Each side carries its own
coverage, source/graph/history provenance and scoring basis. Matching basis means
same producer/scoring versions, biomarker set, applicability and waiver policy;
input generations and rolling windows may differ, because this is a temporal
observation. Explain history/window/config changes in the result; a measured
health change does not by itself establish that the source edit caused it.

Only two complete compatible observations produce a measured delta. No baseline,
new/deleted file, either side partial/missing, legacy provenance, unsupported-set
change, or score-version change produces an explicitly incomparable result with
both observations preserved. Completeness transitions remain visible at equal
numeric scores. Component/workspace measured comparisons also require compatible
membership/population and weighting; otherwise expose constituent changes without
calling the aggregate a measured improvement/degradation. No fallback baseline 10.

Partial scoring returns an interval, not measured degradation. For each category,
the optimistic deduction is capped known current debt; if any applicable producer
is missing, the pessimistic deduction saturates the category cap. Apply normal
global score limits to the summed deductions. This is a conservative bound even
when a missing producer could emit multiple findings; one missing finding's weight
is **not** a proved worst case. Unsupported producers are excluded with reasons.
Never add stale findings to known current debt.

On-demand attribution compares the **same current persistent evidence** with and
without the fresh review findings. Both calculations share the ordinary category
caps with persistent findings. Return raw on-demand deductions, scaled deductions
and their marginal score effect separately: at a saturated cap the marginal effect
can be zero despite a real finding. It is not a temporal delta. Missing on-demand
evidence is explicit; partial persistent evidence permits only conditional/bounded
attribution, not an exact marginal effect. A parse-time snapshot does not contain
blame or shape evidence and cannot be used to invent their historical improvement.

## Migration and implementation slices

Old stamps are Unknown, including old `partial=false` defaults. Do not backfill
current health stamps from content hashes, HEAD, or snapshot completeness. Preserve
legacy snapshots as legacy observations. Re-ingest raw history once after upgrade
even at identical HEAD: some links may already have cascaded away. If unavailable,
retain what remains as unverified; no migration can reconstruct destroyed evidence
by relabelling it. Same-content explicit reparse and health demand both repair.

| Task | Boundary required by this proposal |
|---|---|
| 413 | Stable path identity and full child lifecycle audit; preserve raw history, invalidate health |
| 414 | Epoch/generation/input stamps, per-producer outcomes, atomic run publication, conservative legacy migration |
| 415 | Shared locked refresh, day bucket/history/config probes, fresh rollups, consumer adapters and baseline pinning |
| 416 | Validated scoring, interval bounds, temporal comparison vs cap-sharing attribution, complete real-path regression |
| 417 | Fallible repository probe independent of new schema; preserve Unknown vs confirmed absence |
| 418 | Snapshot/history/trend completeness fidelity, including atomic snapshot writer; no legacy completeness inference |

Implementation regression coverage must include the actual full parse → comment
edit → incremental parse → health/review path; unchanged peers affected by graph
edits; unchanged-HEAD mixed biomarkers; stale rollups; midnight/config/version
changes; failed probe with retained rows; empty per-file history; damaged legacy
indexes; publication races/rollback; partial baseline/current; saturated caps;
component unavailability; and snapshot round-trip through the production writer.

Verified interfaces: `Db::replace_file_data`, `post_parse_sequence`,
`compute_all_health_findings`, `compute_health_delta`, `score_file`,
`compute_hidden_coupling`, `compute_blast_radius_churn`, `compute_change_entropy`,
`git_commit_files`, and `load_owners_config`. The owners loader currently defaults
on parse/read errors; the new input contract must make those failures observable.
See also [the verified lifecycle review](reviews/2026-09-21-health-evidence-lifecycle.md).
Additional verified defect: `Db::insert_snapshot_atomic` at `src/db/mod.rs:2353`
omits `partial` and `missing_biomarkers`; both production snapshot paths use it.
The separate `insert_snapshot_files` stores them. Sutra/418 must fix the production
writer and test its round-trip, not just expose fields already lost at ingestion.

## Types-first skeleton

This is the sole skeleton artifact; extract its Rust block into a temporary
library for `cargo check`. Types borrow existing findings and use static dispatch.
Private construction of sessions, validated inputs and observations belongs to
the future health module. Trait declarations have no implementation bodies.
No new production module is compiled or exported by this design.

Validation on 2026-09-21: extracted this block into a temporary library depending
on this checkout's `sutra` and `thiserror = "2"`; `cargo check --offline` passed.
Only the deliberately body-free score wrappers report unused-field warnings.

```rust
use std::path::Path;
use sutra::db::HealthFindingRow;
use sutra::health::BiomarkerKind;

#[derive(Debug, PartialEq, Eq)]
pub struct Digest([u8; 32]);
#[derive(Debug, PartialEq, Eq)]
pub struct IndexEpoch(Digest);
#[derive(Debug, PartialEq, Eq)]
pub struct Generation(u64);
#[derive(Debug, PartialEq, Eq)]
pub struct RunId(u64);
#[derive(Debug, PartialEq, Eq)]
pub struct CheckpointId(i64);
#[derive(Debug, PartialEq, Eq)]
pub struct UtcDay(i64);

pub struct FileIdentity<'a> {
    pub epoch: &'a IndexEpoch,
    pub path: &'a Path,
}
pub struct GraphStamp {
    pub epoch: IndexEpoch,
    pub generation: Generation,
    pub parser: Digest,
    pub resolver: Digest,
    pub indexed_paths: Digest,
}
pub enum Head { Unborn, Commit(Digest) }
pub struct RepositoryStamp {
    pub identity: Digest,
    pub head: Head,
    pub history_boundaries: Digest,
}
pub struct HistoryStamp {
    pub repository: RepositoryStamp,
    pub day: UtcDay,
    pub window_days: u32,
    pub indexed_paths: Digest,
    pub ingestion_version: Digest,
    pub ingestion_generation: Generation,
}
pub enum RepositoryObservation {
    Present(RepositoryStamp),
    ConfirmedAbsent { probe: Digest },
    Unknown(InputFailure),
}
pub enum HistoryObservation {
    Loaded(HistoryStamp),
    Empty(HistoryStamp),
    Unsupported { absence_probe: Digest },
    Unknown(InputFailure),
}
pub enum ConfigStamp { AbsentDefault(Digest), Parsed(Digest) }
pub enum InputFailure {
    ExtractionIncomplete, ResolutionIncomplete, ProbeFailed,
    IngestionFailed, HistoryIncomplete, ConfigUnreadable, ConfigInvalid,
}
pub struct InputStamp {
    pub graph: GraphStamp,
    pub history: HistoryObservation,
    pub owners: Result<ConfigStamp, InputFailure>,
    pub rollups: Result<Generation, InputFailure>,
    pub analysis_version: Digest,
}
pub struct ComponentStamp {
    pub graph: Generation,
    pub history: Generation,
    pub membership_and_config: Digest,
    pub analysis_version: Digest,
}
pub enum MissingReason {
    NeverComputed, LegacyUnknown, InputsChanged, NoHistory,
    Failed(InputFailure), Deferred(DeferReason),
}
pub enum DeferReason { LockBusy, Frozen }
pub enum UnsupportedReason { ConfirmedNonRepository, NoCoverageIngestion }
pub enum ProducerOutcome {
    Complete { finding_count: usize },
    Missing(MissingReason),
    Unsupported(UnsupportedReason),
}
pub struct ProducerEvidence<'a> {
    pub file: FileIdentity<'a>,
    pub producer: BiomarkerKind,
    pub outcome: ProducerOutcome,
}
pub struct FindingEvidence<'a> {
    pub row: &'a HealthFindingRow,
    pub file: FileIdentity<'a>,
    pub symbol_label: Option<&'a str>,
}
pub struct EvidenceRun<'a> {
    pub id: RunId,
    pub inputs: InputStamp,
    pub outcomes: &'a [ProducerEvidence<'a>],
    pub findings: &'a [FindingEvidence<'a>],
}
pub enum Validity { Current, Stale(MissingReason) }
pub trait HealthValidity {
    fn validate(&self, recorded: &InputStamp, observed: &InputStamp) -> Validity;
}
pub struct RetainedEvidence<'a> {
    pub run: &'a EvidenceRun<'a>,
    pub validity: Validity,
}
pub enum RefreshResult<'a> {
    Reused(&'a EvidenceRun<'a>),
    Published(&'a EvidenceRun<'a>), // May contain explicit partial outcomes.
    Deferred { reason: DeferReason, retained: Option<RetainedEvidence<'a>> },
    Failed { error: HealthError, retained: Option<RetainedEvidence<'a>> },
}
#[derive(Debug, thiserror::Error)]
pub enum HealthError {
    #[error("health storage: {0}")]
    Storage(#[source] sutra::error::SutraError),
    #[error("inputs changed during health publication")]
    InputsChanged,
    #[error("health worker was interrupted")]
    WorkerInterrupted,
    #[error("invalid persisted health evidence: {0}")]
    InvalidEvidence(String),
}

// Opaque witnesses: only the coordinator adapter can construct a session;
// only validation can construct Publishable. No nested coordinator acquisition.
pub struct HealthSession<'lock> { _exclusive: &'lock mut () }
pub struct Publishable<'a> { _run: EvidenceRun<'a> }
pub trait HealthRefresh {
    fn refresh<'a>(
        &'a mut self, session: &mut HealthSession<'_>, root: &Path, day: UtcDay,
    ) -> RefreshResult<'a>;
    fn publish(
        &mut self, session: &mut HealthSession<'_>, run: Publishable<'_>,
    ) -> Result<RunId, HealthError>;
}

pub struct ScoreBasis {
    pub scoring_version: Digest,
    pub producer_versions: Digest,
    pub biomarkers: Digest,
    pub applicability: Digest,
    pub waiver_policy: Digest,
    pub population_and_weights: Digest,
}
pub struct MeasuredScore(f64); // Validated finite value in the score range.
pub struct ScoreBounds { lower: f64, upper: f64 }
pub enum ScoreValue {
    Measured(MeasuredScore),
    Partial { bounds: ScoreBounds, missing: Vec<BiomarkerKind> },
    Unavailable(MissingReason),
}
pub enum Provenance<'a> {
    Run(&'a EvidenceRun<'a>),
    Checkpoint { id: &'a CheckpointId, inputs: &'a InputStamp },
    Legacy(&'a CheckpointId),
}
pub struct Observation<'a> {
    pub provenance: Provenance<'a>,
    pub basis: &'a ScoreBasis,
    pub value: ScoreValue,
}
pub enum ComparisonSide<'a> {
    Observed(Observation<'a>), NoBaseline, FileAbsent,
}
pub enum IncomparableReason {
    MissingSide, PartialEvidence, LegacyProvenance, BasisChanged,
}
pub enum TemporalChange {
    Measured { delta: f64 },
    Incomparable(IncomparableReason),
}
pub struct TemporalComparison<'a> {
    pub previous: ComparisonSide<'a>,
    pub current: ComparisonSide<'a>,
    pub change: TemporalChange,
}
pub struct ReviewStamp {
    pub repository: RepositoryStamp,
    pub base: Digest,
    pub source_and_extraction: Digest,
    pub analysis_version: Digest,
}
pub struct OnDemandEvidence<'a> {
    pub stamp: ReviewStamp,
    pub outcomes: &'a [ProducerEvidence<'a>],
    pub findings: &'a [HealthFindingRow],
}
pub enum MarginalEffect {
    Exact(f64), Conditional { lower: f64, upper: f64 }, Unavailable,
}
pub struct FindingAttribution {
    pub finding_id: i64,
    pub raw_deduction: f64,
    pub scaled_deduction: f64,
}
pub struct OnDemandAttribution {
    pub without: ScoreValue,
    pub with: ScoreValue,
    pub effect: MarginalEffect,
    pub findings: Vec<FindingAttribution>,
}
pub trait HealthComparison {
    fn temporal<'a>(
        &self, previous: ComparisonSide<'a>, current: ComparisonSide<'a>,
    ) -> Result<TemporalComparison<'a>, HealthError>;
    fn attribute(
        &self, persistent: &Observation<'_>, fresh: &OnDemandEvidence<'_>,
    ) -> Result<OnDemandAttribution, HealthError>;
}
```

All storage/worker/input-race failures are recoverable. Producer failures are
data in outcomes. No intentional panic, unsafe, dynamic dispatch or shared mutable
heap ownership is required. The opaque session is a design witness, not a second
mutex; implementation must tie its private constructor to the real held guard.

## Decisions to review

- Day-quantized expiry trades sub-day window precision for deterministic cheap
  reuse. Rejected exact-second expiry would need boundary scheduling and more
  frequent refreshes; HEAD-only reuse is incorrect.
- Missing per-file history stays partial, and partial scores are cap-based bounds.
  Rejected successful-empty-as-clean would turn lack of observations into health.
- File health refresh leaves stale component scores unavailable. Rejected eager
  clustering would put unrelated expensive work on the demand path.
- Temporal deltas require complete compatible evidence; on-demand attribution is
  separate. Rejected baseline mirroring/washing hides lost evidence and cannot
  support measured improvement.
