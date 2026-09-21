//! Persisted health-evidence model (sutra/414).
//!
//! This module implements the storage-facing subset of the approved health
//! evidence contract (`docs/health-evidence-contract.md`, sutra/412): the value
//! types that identify *what inputs a health observation was computed against*,
//! the conservative validity comparison that decides whether a retained
//! observation is still current, and the owned run/outcome/finding types that
//! [`crate::db`] round-trips through the immutable `health_runs` store.
//!
//! Deliberately **out of scope** here (later slices): the locked refresh
//! orchestration and the borrowed staging/`HealthRefresh` trait (sutra/415), and
//! the scoring/comparison/attribution layer (sutra/416). Keeping those out avoids
//! compiling dead code and premature commitment; this slice is schema + typed
//! validity + atomic publication only.
//!
//! ## Design notes vs. the contract skeleton
//! - `Generation` and `RunId` are `i64`, not the skeleton's `u64`: they map onto
//!   `index_meta.data_generation` (i64, non-negative) and the sqlite rowid
//!   directly, so there is no lossy cast at the DB boundary.
//! - A run's `InputStamp`, per-producer outcomes and retained findings persist as
//!   JSON blobs. The contract forbids immutable evidence from cascading through
//!   live file/symbol foreign keys and treats numeric IDs as diagnostic-only; a
//!   blob has no FK by construction and `validate` only ever compares stamps in
//!   Rust, never in SQL.

use serde::{Deserialize, Serialize};

use crate::db::HealthFindingRow;
use crate::error::SutraError;
use crate::health::BiomarkerKind;

/// A 32-byte content digest. Serialized as a lowercase hex string so persisted
/// evidence blobs stay readable and comparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Digest(pub [u8; 32]);

impl Digest {
    /// Digest of arbitrary content (blake3). Use this to fingerprint the inputs
    /// a producer consumed — a config file's parsed bytes, the sorted indexed
    /// path set, a version string.
    pub fn of(bytes: &[u8]) -> Self {
        Digest(*blake3::hash(bytes).as_bytes())
    }

    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(64);
        for b in self.0 {
            use std::fmt::Write;
            let _ = write!(s, "{b:02x}");
        }
        s
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        // Require 64 ASCII bytes before slicing: `s.len()` counts bytes, so a
        // 64-byte value carrying a multi-byte UTF-8 char would slice across a
        // char boundary and panic. Persisted digests are always ASCII hex;
        // anything else is malformed and returns None (recoverable), not a panic.
        if s.len() != 64 || !s.is_ascii() {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Digest(out))
    }
}

impl Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Digest::from_hex(&s).ok_or_else(|| serde::de::Error::custom("invalid digest hex"))
    }
}

/// Identifies one lifetime of the index. Regenerated on every full reindex, so a
/// retained numeric ID from a prior epoch is never re-resolved against
/// replacement extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEpoch(pub Digest);

/// Conservative workspace graph generation (mirrors `index_meta.data_generation`,
/// bumped in the same transaction as every extraction/resolution mutation). Its
/// existence alone does not prove complete coverage — that is why a run also
/// records parser identity and per-producer outcomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Generation(pub i64);

/// Immutable run identifier (the `health_runs` rowid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunId(pub i64);

/// A UTC-day bucket (`floor(t / 86400)`). History ingestion is quantized to this
/// bucket; a clock crossing midnight invalidates git evidence even at an
/// unchanged HEAD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UtcDay(pub i64);

/// Pinned repository HEAD, including the unborn-branch state (a fresh repo with
/// no commits is a successful observation of no history, not a failure).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Head {
    Unborn,
    Commit(Digest),
}

/// Repository/worktree identity checked before publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryStamp {
    pub identity: Digest,
    pub head: Head,
    /// Shallow/replacement boundary fingerprint: an incomplete object graph
    /// yields different history and must invalidate.
    pub history_boundaries: Digest,
}

/// The full identity of a mapped history window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryStamp {
    pub repository: RepositoryStamp,
    pub day: UtcDay,
    pub window_days: u32,
    pub indexed_paths: Digest,
    pub ingestion_version: Digest,
    pub ingestion_generation: Generation,
}

/// Why an input could not be established. Distinguishes "looked and it is
/// genuinely absent/empty" (recorded elsewhere) from "failed to look" — a failed
/// probe is never silently treated as a clean/empty observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputFailure {
    ExtractionIncomplete,
    ResolutionIncomplete,
    ProbeFailed,
    IngestionFailed,
    HistoryIncomplete,
    ConfigUnreadable,
    ConfigInvalid,
}

/// Observed repository state. Only [`RepositoryObservation::ConfirmedAbsent`]
/// makes git biomarkers unsupported; an arbitrary failure is `Unknown`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RepositoryObservation {
    Present(RepositoryStamp),
    ConfirmedAbsent { probe: Digest },
    Unknown(InputFailure),
}

/// Observed mapped history. `Empty` is a successful observation of no usable
/// history (git producers are `NoHistory`, structural producers stay current);
/// `Unknown` is a failed probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryObservation {
    Loaded(HistoryStamp),
    Empty(HistoryStamp),
    Unsupported { absence_probe: Digest },
    Unknown(InputFailure),
}

/// Ownership configuration identity. Absence has a distinct default digest;
/// malformed data is a failure ([`InputFailure::ConfigInvalid`]), never an empty
/// default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigStamp {
    AbsentDefault(Digest),
    Parsed(Digest),
}

/// Identity of the graph inputs shared by every structural producer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphStamp {
    pub epoch: IndexEpoch,
    pub generation: Generation,
    pub parser: Digest,
    pub resolver: Digest,
    pub indexed_paths: Digest,
}

/// The complete input identity a health run was computed against. Persisted with
/// the run and re-derived at query time; [`validate`] compares the two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputStamp {
    pub graph: GraphStamp,
    pub history: HistoryObservation,
    pub owners: Result<ConfigStamp, InputFailure>,
    pub rollups: Result<Generation, InputFailure>,
    pub analysis_version: Digest,
}

/// Why a producer's current evidence is missing rather than measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MissingReason {
    NeverComputed,
    LegacyUnknown,
    InputsChanged,
    NoHistory,
    Failed(InputFailure),
    Deferred(DeferReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeferReason {
    LockBusy,
    Frozen,
}

/// A producer excluded from scoring because its data source is structurally
/// absent, not merely unavailable this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnsupportedReason {
    ConfirmedNonRepository,
    NoCoverageIngestion,
}

/// The outcome of one producer for one file. `Complete { finding_count: 0 }` is a
/// successful empty observation and is explicitly distinct from `Missing`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProducerOutcome {
    Complete { finding_count: usize },
    Missing(MissingReason),
    Unsupported(UnsupportedReason),
}

/// Whether a retained observation still reflects current inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Validity {
    Current,
    Stale(MissingReason),
}

/// Owned per-producer/per-file outcome, as persisted in a run. The captured
/// `file_path` is authoritative for display; any numeric id lives only inside the
/// retained finding rows and is diagnostic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredOutcome {
    pub file_path: String,
    pub producer: BiomarkerKind,
    pub outcome: ProducerOutcome,
}

/// Owned retained finding. Carries the finding row plus the captured
/// path/symbol label used for display; deleting live extraction must not rewrite
/// this provenance, so nothing here is re-resolved against replacement rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredFinding {
    pub finding: HealthFindingRow,
    pub file_path: String,
    pub symbol_label: Option<String>,
}

/// One immutable health run, as round-tripped through the `health_runs` store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRun {
    pub id: RunId,
    pub index_epoch: IndexEpoch,
    pub graph_generation: Generation,
    pub inputs: InputStamp,
    pub outcomes: Vec<StoredOutcome>,
    pub findings: Vec<StoredFinding>,
    pub created_at: String,
}

/// The staged content of a run to publish. Identical payload to [`StoredRun`]
/// minus the storage-assigned `id`/`created_at`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublishRun {
    pub index_epoch: IndexEpoch,
    pub graph_generation: Generation,
    pub inputs: InputStamp,
    pub outcomes: Vec<StoredOutcome>,
    pub findings: Vec<StoredFinding>,
}

/// Recoverable errors for the health evidence layer. Producer-local failures are
/// *data* in outcomes, never errors; these are storage/worker/race failures.
#[derive(Debug, thiserror::Error)]
pub enum HealthError {
    #[error("health storage: {0}")]
    Storage(#[source] SutraError),
    #[error("inputs changed during health publication")]
    InputsChanged,
    #[error("health worker was interrupted")]
    WorkerInterrupted,
    #[error("invalid persisted health evidence: {0}")]
    InvalidEvidence(String),
}

impl From<SutraError> for HealthError {
    fn from(e: SutraError) -> Self {
        HealthError::Storage(e)
    }
}

/// Conservative validity: a retained observation is [`Validity::Current`] only
/// when every recorded input axis still matches what is observed now. The first
/// diverging axis decides the [`MissingReason`]:
///
/// - an observed *failure* on an axis the run recorded as established →
///   `Stale(Failed(..))` (we cannot re-establish the input);
/// - an observed *absence of usable history* where the run had history →
///   `Stale(NoHistory)`;
/// - any other difference (bumped generation, crossed midnight, changed window,
///   HEAD move, config edit, version bump) → `Stale(InputsChanged)`.
///
/// Axis order is graph → analysis version → history → owners → rollups, chosen so
/// the most fundamental change (a graph mutation) is reported first.
pub fn validate(recorded: &InputStamp, observed: &InputStamp) -> Validity {
    // Graph identity: epoch, generation, parser, resolver, indexed paths. Any
    // mismatch (including an unchanged file whose *graph* moved) is stale.
    if recorded.graph != observed.graph {
        return Validity::Stale(MissingReason::InputsChanged);
    }
    if recorded.analysis_version != observed.analysis_version {
        return Validity::Stale(MissingReason::InputsChanged);
    }
    if let Some(reason) = history_validity(&recorded.history, &observed.history) {
        return Validity::Stale(reason);
    }
    if let Some(reason) = result_validity(&recorded.owners, &observed.owners) {
        return Validity::Stale(reason);
    }
    if let Some(reason) = result_validity(&recorded.rollups, &observed.rollups) {
        return Validity::Stale(reason);
    }
    Validity::Current
}

/// `None` when the observed history still matches; otherwise the reason it is
/// stale. A run's history is either `Loaded` or `Empty` (both carry a stamp); an
/// observed `Unknown` is a failed probe, an observed `Unsupported`/differing
/// stamp is a real change.
fn history_validity(
    recorded: &HistoryObservation,
    observed: &HistoryObservation,
) -> Option<MissingReason> {
    // A failed probe *now* is never a basis for reuse, even when the recorded
    // stamp captured the identical failure: we cannot confirm the current
    // inputs, so we must refresh (conservative-on-unknown, health-evidence
    // contract). Check the observed failure before the equality short-circuit,
    // otherwise two matching `Unknown` stamps would validate as Current.
    if let HistoryObservation::Unknown(failure) = observed {
        return Some(MissingReason::Failed(*failure));
    }
    if recorded == observed {
        return None;
    }
    match observed {
        HistoryObservation::Unknown(failure) => Some(MissingReason::Failed(*failure)),
        // The window is now empty / absent where the run had usable history.
        HistoryObservation::Empty(_) | HistoryObservation::Unsupported { .. } => {
            Some(MissingReason::NoHistory)
        }
        // A different loaded stamp: HEAD moved, window changed, midnight crossed,
        // ingestion re-versioned, indexed paths changed, ...
        HistoryObservation::Loaded(_) => Some(MissingReason::InputsChanged),
    }
}

/// Validity for a `Result<T, InputFailure>` input axis (owners, rollups): an
/// observed `Err` on an axis the run had `Ok` is a failed probe; a changed `Ok`
/// value is an input change.
fn result_validity<T: PartialEq>(
    recorded: &Result<T, InputFailure>,
    observed: &Result<T, InputFailure>,
) -> Option<MissingReason> {
    // A current probe failure never supports reuse, even if the recorded stamp
    // held the identical error (conservative-on-unknown). Check before the
    // equality short-circuit so two matching `Err` stamps don't pass as Current.
    if let Err(failure) = observed {
        return Some(MissingReason::Failed(*failure));
    }
    if recorded == observed {
        return None;
    }
    match observed {
        Err(failure) => Some(MissingReason::Failed(*failure)),
        Ok(_) => Some(MissingReason::InputsChanged),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(seed: &str) -> Digest {
        Digest::of(seed.as_bytes())
    }

    fn repo() -> RepositoryStamp {
        RepositoryStamp {
            identity: d("repo"),
            head: Head::Commit(d("head")),
            history_boundaries: d("bounds"),
        }
    }

    fn history_stamp() -> HistoryStamp {
        HistoryStamp {
            repository: repo(),
            day: UtcDay(20000),
            window_days: 90,
            indexed_paths: d("paths"),
            ingestion_version: d("ingv1"),
            ingestion_generation: Generation(3),
        }
    }

    fn base() -> InputStamp {
        InputStamp {
            graph: GraphStamp {
                epoch: IndexEpoch(d("epoch")),
                generation: Generation(7),
                parser: d("parser"),
                resolver: d("resolver"),
                indexed_paths: d("paths"),
            },
            history: HistoryObservation::Loaded(history_stamp()),
            owners: Ok(ConfigStamp::Parsed(d("owners"))),
            rollups: Ok(Generation(7)),
            analysis_version: d("analysis"),
        }
    }

    #[test]
    fn identical_stamps_are_current() {
        assert_eq!(validate(&base(), &base()), Validity::Current);
    }

    #[test]
    fn digest_hex_roundtrips() {
        let digest = d("anything");
        assert_eq!(Digest::from_hex(&digest.to_hex()), Some(digest));
        assert_eq!(digest.to_hex().len(), 64);
    }

    #[test]
    fn bumped_generation_invalidates_even_when_files_unchanged() {
        let recorded = base();
        let mut observed = base();
        observed.graph.generation = Generation(8);
        assert_eq!(
            validate(&recorded, &observed),
            Validity::Stale(MissingReason::InputsChanged)
        );
    }

    #[test]
    fn parser_change_invalidates() {
        let recorded = base();
        let mut observed = base();
        observed.graph.parser = d("parser-v2");
        assert_eq!(
            validate(&recorded, &observed),
            Validity::Stale(MissingReason::InputsChanged)
        );
    }

    #[test]
    fn crossing_midnight_invalidates_history() {
        let recorded = base();
        let mut stamp = history_stamp();
        stamp.day = UtcDay(20001);
        let mut observed = base();
        observed.history = HistoryObservation::Loaded(stamp);
        assert_eq!(
            validate(&recorded, &observed),
            Validity::Stale(MissingReason::InputsChanged)
        );
    }

    #[test]
    fn window_length_change_invalidates_history() {
        let recorded = base();
        let mut stamp = history_stamp();
        stamp.window_days = 60;
        let mut observed = base();
        observed.history = HistoryObservation::Loaded(stamp);
        assert_eq!(
            validate(&recorded, &observed),
            Validity::Stale(MissingReason::InputsChanged)
        );
    }

    #[test]
    fn head_move_invalidates_history() {
        let recorded = base();
        let mut stamp = history_stamp();
        stamp.repository.head = Head::Commit(d("head-2"));
        let mut observed = base();
        observed.history = HistoryObservation::Loaded(stamp);
        assert_eq!(
            validate(&recorded, &observed),
            Validity::Stale(MissingReason::InputsChanged)
        );
    }

    #[test]
    fn history_probe_failure_is_failed_not_nohistory() {
        let recorded = base();
        let mut observed = base();
        observed.history = HistoryObservation::Unknown(InputFailure::ProbeFailed);
        assert_eq!(
            validate(&recorded, &observed),
            Validity::Stale(MissingReason::Failed(InputFailure::ProbeFailed))
        );
    }

    #[test]
    fn history_becoming_empty_is_nohistory() {
        let recorded = base();
        let mut observed = base();
        observed.history = HistoryObservation::Empty(history_stamp());
        assert_eq!(
            validate(&recorded, &observed),
            Validity::Stale(MissingReason::NoHistory)
        );
    }

    #[test]
    fn owners_edit_invalidates() {
        let recorded = base();
        let mut observed = base();
        observed.owners = Ok(ConfigStamp::Parsed(d("owners-v2")));
        assert_eq!(
            validate(&recorded, &observed),
            Validity::Stale(MissingReason::InputsChanged)
        );
    }

    #[test]
    fn owners_becoming_unreadable_is_failed() {
        let recorded = base();
        let mut observed = base();
        observed.owners = Err(InputFailure::ConfigUnreadable);
        assert_eq!(
            validate(&recorded, &observed),
            Validity::Stale(MissingReason::Failed(InputFailure::ConfigUnreadable))
        );
    }

    #[test]
    fn rollup_regeneration_invalidates() {
        let recorded = base();
        let mut observed = base();
        observed.rollups = Ok(Generation(8));
        assert_eq!(
            validate(&recorded, &observed),
            Validity::Stale(MissingReason::InputsChanged)
        );
    }

    #[test]
    fn input_stamp_json_roundtrips() {
        let stamp = base();
        let json = serde_json::to_string(&stamp).expect("serialize");
        let back: InputStamp = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(stamp, back);
    }

    #[test]
    fn repeated_history_probe_failure_is_not_current() {
        // Recorded and observed carry the *same* failed history probe. Equality
        // must not license reuse: an unavailable prerequisite forces refresh
        // (conservative-on-unknown), so validate(stamp, stamp) is Stale.
        let mut stamp = base();
        stamp.history = HistoryObservation::Unknown(InputFailure::ProbeFailed);
        assert_eq!(
            validate(&stamp, &stamp),
            Validity::Stale(MissingReason::Failed(InputFailure::ProbeFailed))
        );
    }

    #[test]
    fn repeated_owners_failure_is_not_current() {
        let mut stamp = base();
        stamp.owners = Err(InputFailure::ConfigUnreadable);
        assert_eq!(
            validate(&stamp, &stamp),
            Validity::Stale(MissingReason::Failed(InputFailure::ConfigUnreadable))
        );
    }

    #[test]
    fn repeated_rollup_failure_is_not_current() {
        let mut stamp = base();
        stamp.rollups = Err(InputFailure::ResolutionIncomplete);
        assert_eq!(
            validate(&stamp, &stamp),
            Validity::Stale(MissingReason::Failed(InputFailure::ResolutionIncomplete))
        );
    }

    #[test]
    fn from_hex_rejects_non_ascii_without_panicking() {
        // A 64-*byte* value that is not ASCII must return None, never panic by
        // slicing across a UTF-8 char boundary. "€" is 3 bytes; one plus 61
        // ASCII zeros is 64 bytes but 62 chars, so the old byte-offset slicing
        // would split the multi-byte char.
        let malformed = format!("€{}", "0".repeat(61));
        assert_eq!(malformed.len(), 64);
        assert!(!malformed.is_ascii());
        assert_eq!(Digest::from_hex(&malformed), None);
    }
}
