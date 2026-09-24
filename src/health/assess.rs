//! Validated persistent health observations (sutra/416).
//!
//! Every scoring consumer — file health, the snapshot writer, review — reads the
//! persistent side through [`PersistentEvidence::load`]: the current immutable
//! health run, its per-(file, producer) outcomes, and its retained findings, all
//! from one coherent read. Nothing here trusts a coverage bool, a content hash, or
//! a live-table finding count (health-evidence-contract.md § Publication and
//! consumers: "consume validated observations, not a raw coverage bool; no
//! finding-count escape hatch").
//!
//! The caller supplies a [`RunVerdict`] — the validity of one *specific* run,
//! established by the demand refresh it just performed or by
//! [`crate::health::refresh::current_run_validity`]. If the current pointer has
//! moved to a different run since, the verdict does not transfer and the loaded
//! run reads as stale. A stale run is still loaded (its findings stay visible)
//! but every outcome that depends on the moved inputs becomes `Missing`, so
//! stale findings can never be counted as known current debt.

use std::collections::HashMap;

use crate::db::{Db, HealthFindingRow, HealthWaiverRow};
use crate::error::Result;
use crate::health::evidence::{
    Digest, MissingReason, ProducerOutcome, RunId, StoredFinding, UnsupportedReason, Validity,
};
use crate::health::findings::{BiomarkerKind, HealthSeverity};
use crate::health::instability::{self, ComponentInstability};
use crate::health::scoring::{
    self, EvidencePart, FileHealthScore, PERSISTENT_PRODUCERS, ProducerResult, ScoreValue,
};
use crate::waivers::{self, ResolvedHealthFinding};

/// Component-aggregation identity folded into every component basis. Bump when
/// the aggregation rule (NLOC weighting, instability penalty form) changes.
const COMPONENT_SCORING_VERSION: &str = "component-scoring-v1-nloc-instability";

/// A validity verdict about one specific run. `Current` vouches only for `run`;
/// it never transfers to whichever run the pointer names at load time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunVerdict {
    pub run: Option<RunId>,
    pub validity: Validity,
}

impl RunVerdict {
    pub fn stale(reason: MissingReason) -> Self {
        RunVerdict {
            run: None,
            validity: Validity::Stale(reason),
        }
    }
}

/// Capture each row's file path and symbol label — the identity health waivers
/// match on. The one labelling step shared by the persistent run
/// ([`crate::health::refresh::publish_run`]), on-demand review and the live-table
/// diagnostic, so their waiver identity cannot drift (sutra/437). Labels are
/// looked up, never consumed: several findings can share one symbol. A file id
/// missing from `path_by_id` labels as `"?"`, which no waiver path matches.
pub fn label_findings<S: AsRef<str>>(
    db: &Db,
    rows: Vec<HealthFindingRow>,
    path_by_id: &HashMap<i64, S>,
) -> Result<Vec<StoredFinding>> {
    let symbol_ids: Vec<i64> = rows.iter().filter_map(|r| r.symbol_id).collect();
    let labels = db.symbol_labels(&symbol_ids)?;
    Ok(rows
        .into_iter()
        .map(|row| StoredFinding {
            file_path: path_by_id
                .get(&row.file_id)
                .map_or_else(|| "?".to_string(), |p| p.as_ref().to_string()),
            symbol_label: row.symbol_id.and_then(|sid| labels.get(&sid).cloned()),
            finding: row,
        })
        .collect())
}

/// The persistent evidence for one currently indexed file.
#[derive(Debug)]
pub struct FileEvidence {
    pub path: String,
    /// Live file id (display and component membership only).
    pub file_id: i64,
    /// One outcome per [`PERSISTENT_PRODUCERS`] entry, validity applied.
    pub outcomes: Vec<ProducerResult>,
    /// Retained findings not covered by a waiver.
    pub findings: Vec<HealthFindingRow>,
    /// Retained findings a waiver excludes from scoring.
    pub waived: Vec<HealthFindingRow>,
    /// [`scoring::file_score_basis`] of this observation.
    pub basis: Digest,
}

impl FileEvidence {
    pub fn part(&self) -> EvidencePart<'_> {
        EvidencePart {
            outcomes: &self.outcomes,
            findings: &self.findings,
        }
    }

    pub fn score(&self) -> FileHealthScore {
        scoring::score_file(&[self.part()])
    }
}

/// The current persistent health observation for every indexed file.
#[derive(Debug)]
pub struct PersistentEvidence {
    /// The run read, or `None` on an index that never published one (legacy).
    pub run_id: Option<RunId>,
    pub validity: Validity,
    /// Every currently indexed file, in `Db::all_files` order.
    pub files: Vec<FileEvidence>,
    by_path: HashMap<String, usize>,
    /// Waivers, retained so on-demand findings get the same policy.
    waivers: Vec<HealthWaiverRow>,
}

impl PersistentEvidence {
    pub fn load(db: &Db, verdict: RunVerdict) -> Result<Self> {
        let run = db.load_current_health_run()?;
        let waivers = db.get_health_waivers()?;
        let indexed = db.all_files()?;

        // A `Current` verdict vouches for the run it was established against.
        // If the pointer moved since (a concurrent refresh published another
        // run), that run is unverified here.
        let validity = match (verdict.validity, &run) {
            (Validity::Current, Some(r)) if verdict.run == Some(r.id) => Validity::Current,
            (Validity::Current, Some(_)) => Validity::Stale(MissingReason::InputsChanged),
            (Validity::Current, None) => Validity::Stale(MissingReason::LegacyUnknown),
            (stale, _) => stale,
        };

        let (run_id, stored_outcomes, stored_findings) = match run {
            Some(run) => (Some(run.id), run.outcomes, run.findings),
            None => (None, Vec::new(), Vec::new()),
        };
        // No run at all is a legacy/never-analyzed index; a run that simply lacks
        // a file (added since) never computed it.
        let absent = if run_id.is_some() {
            MissingReason::NeverComputed
        } else {
            MissingReason::LegacyUnknown
        };

        let mut outcomes_by_path: HashMap<String, Vec<ProducerResult>> = HashMap::new();
        for o in stored_outcomes {
            outcomes_by_path
                .entry(o.file_path)
                .or_default()
                .push((o.producer, o.outcome));
        }

        // Waivers match on the captured path/symbol label, never on re-resolved
        // live ids (contract § Identity and validity).
        let resolved: Vec<ResolvedHealthFinding> = stored_findings
            .into_iter()
            .map(ResolvedHealthFinding::from)
            .collect();
        let (active, waived) = waivers::partition(resolved, &waivers);
        let mut active_by_path: HashMap<String, Vec<HealthFindingRow>> = HashMap::new();
        for r in active {
            active_by_path
                .entry(r.file_path)
                .or_default()
                .push(r.finding);
        }
        let mut waived_by_path: HashMap<String, Vec<HealthFindingRow>> = HashMap::new();
        for w in waived {
            waived_by_path
                .entry(w.finding.file_path)
                .or_default()
                .push(w.finding.finding);
        }

        let mut files = Vec::with_capacity(indexed.len());
        let mut by_path = HashMap::with_capacity(indexed.len());
        for file in &indexed {
            let path: &str = &file.path;
            let recorded = outcomes_by_path.remove(path).unwrap_or_default();
            let findings = active_by_path.remove(path).unwrap_or_default();
            let waived = waived_by_path.remove(path).unwrap_or_default();
            let outcomes: Vec<ProducerResult> = PERSISTENT_PRODUCERS
                .iter()
                .map(|&kind| {
                    let stored = recorded
                        .iter()
                        .find(|(k, _)| *k == kind)
                        .map(|(_, o)| *o)
                        .unwrap_or(ProducerOutcome::Missing(absent));
                    let stored = check_retained(stored, kind, &findings, &waived);
                    (kind, apply_validity(stored, validity))
                })
                .collect();
            let file_waivers: Vec<&HealthWaiverRow> =
                waivers.iter().filter(|w| w.file_path == path).collect();
            let basis = scoring::file_score_basis(&outcomes, &file_waivers);
            by_path.insert(path.to_string(), files.len());
            files.push(FileEvidence {
                path: path.to_string(),
                file_id: file.id,
                outcomes,
                findings,
                waived,
                basis,
            });
        }

        Ok(Self {
            run_id,
            validity,
            files,
            by_path,
            waivers,
        })
    }

    pub fn file(&self, path: &str) -> Option<&FileEvidence> {
        self.by_path.get(path).map(|&i| &self.files[i])
    }

    pub fn waivers(&self) -> &[HealthWaiverRow] {
        &self.waivers
    }

    /// Distinct structurally-unsupported producers across the workspace.
    pub fn unsupported(&self) -> Vec<(BiomarkerKind, UnsupportedReason)> {
        let mut out: Vec<(BiomarkerKind, UnsupportedReason)> = Vec::new();
        for f in &self.files {
            for &(kind, outcome) in &f.outcomes {
                if let ProducerOutcome::Unsupported(reason) = outcome
                    && !out.iter().any(|(k, r)| *k == kind && *r == reason)
                {
                    out.push((kind, reason));
                }
            }
        }
        out
    }
}

/// A `Complete` outcome is trusted only when the run actually retained that
/// many findings for the producer (active + waived) and every one parses;
/// otherwise the run is internally inconsistent for it: `Missing(InvalidEvidence)`.
fn check_retained(
    outcome: ProducerOutcome,
    kind: BiomarkerKind,
    active: &[HealthFindingRow],
    waived: &[HealthFindingRow],
) -> ProducerOutcome {
    let ProducerOutcome::Complete { finding_count } = outcome else {
        return outcome;
    };
    let mut count = 0;
    for f in active.iter().chain(waived) {
        if BiomarkerKind::parse(&f.biomarker_kind) != Some(kind) {
            continue;
        }
        if HealthSeverity::parse(&f.severity).is_none() {
            return ProducerOutcome::Missing(MissingReason::InvalidEvidence);
        }
        count += 1;
    }
    if count == finding_count {
        outcome
    } else {
        ProducerOutcome::Missing(MissingReason::InvalidEvidence)
    }
}

/// A stale run cannot vouch for anything that depends on the moved inputs. Every
/// `Complete` and every repository-dependent `Unsupported` becomes `Missing` with
/// the staleness reason; only the build-structural `NoCoverageIngestion` stays
/// unsupported (no build of sutra can observe coverage).
fn apply_validity(outcome: ProducerOutcome, validity: Validity) -> ProducerOutcome {
    match validity {
        Validity::Current => outcome,
        Validity::Stale(reason) => match outcome {
            ProducerOutcome::Unsupported(UnsupportedReason::NoCoverageIngestion) => outcome,
            _ => ProducerOutcome::Missing(reason),
        },
    }
}

/// One scored file of a workspace pass.
#[derive(Debug)]
pub struct ScoredFile<'e> {
    pub evidence: &'e FileEvidence,
    pub score: FileHealthScore,
}

#[derive(Debug)]
pub struct ScoredComponent<'e> {
    pub component_id: String,
    pub component_name: String,
    /// NLOC-weighted member scores minus the instability penalty. `Measured` only
    /// when every member file is measured.
    pub value: ScoreValue,
    pub member_count: usize,
    pub total_nloc: i64,
    pub instability: Option<ComponentInstability>,
    /// The instability penalty subtracted, or `None` when instability could not
    /// be computed (the value is then bounded, never measured).
    pub penalty: Option<f64>,
    /// Each observed member's path and aggregation weight (line count). Not in
    /// the basis: trend measures at the baseline's weights and reports the
    /// weight shift separately (sutra/436).
    pub members: Vec<(&'e str, i64)>,
    /// Every member file id (live files, including unscored ones) — the same
    /// set `member_count` counts. Lets callers aggregate per-file data over the
    /// component without re-querying membership.
    pub member_file_ids: Vec<i64>,
    /// Membership + member bases + aggregation identity: two component
    /// observations compare as measured only when this matches.
    pub basis: Digest,
}

#[derive(Debug)]
pub struct WorkspaceHealth<'e> {
    pub files: Vec<ScoredFile<'e>>,
    pub components: Vec<ScoredComponent<'e>>,
}

/// Score every indexed file and every live component from validated evidence.
///
/// Components are measured only when their prerequisites are current:
/// `membership_current` (the clustering matches the live graph/history/config —
/// a health-only refresh never re-clusters) and a successful instability
/// computation. Otherwise the component is `Partial`: stale membership keeps its
/// bounds but is never measured; an instability failure widens the lower bound by
/// the maximum penalty rather than failing the whole report.
pub fn score_workspace<'e>(
    db: &Db,
    evidence: &'e PersistentEvidence,
    membership_current: bool,
) -> Result<WorkspaceHealth<'e>> {
    let files: Vec<ScoredFile<'e>> = evidence
        .files
        .iter()
        .map(|e| ScoredFile {
            evidence: e,
            score: e.score(),
        })
        .collect();
    let by_id: HashMap<i64, &ScoredFile<'e>> =
        files.iter().map(|f| (f.evidence.file_id, f)).collect();

    let memberships = db.component_members_with_line_count()?;
    // Instability feeds the component score. A failure is not silently a zero
    // penalty: it leaves the penalty unknown, which `score_members` bounds.
    let mut instability_map = match instability::compute_component_instability(db) {
        Ok(map) => Some(map),
        Err(e) => {
            tracing::warn!("health: component instability unavailable: {e}");
            None
        }
    };

    let mut members_of: HashMap<&str, Vec<(i64, i64)>> = HashMap::new();
    for (comp_id, file_id, line_count) in &memberships {
        members_of
            .entry(comp_id.as_str())
            .or_default()
            .push((*file_id, *line_count));
    }

    let mut components = Vec::new();
    for comp in db.all_components()? {
        let Some(members) = members_of.get(comp.id.as_str()) else {
            continue;
        };
        let instability = instability_map
            .as_mut()
            .map(|m| Penalty::Known(m.remove(&comp.id)))
            .unwrap_or(Penalty::Unknown);
        let (value, basis) = score_members(members, &by_id, &instability, membership_current);
        let penalty = instability.known();
        let observed = members
            .iter()
            .filter_map(|(fid, nloc)| by_id.get(fid).map(|f| (f.evidence.path.as_str(), *nloc)))
            .collect();
        let instability = match instability {
            Penalty::Known(i) => i,
            Penalty::Unknown => None,
        };
        components.push(ScoredComponent {
            component_id: comp.id,
            component_name: comp.name,
            value,
            member_count: members.len(),
            total_nloc: members.iter().map(|(_, n)| n).sum(),
            instability,
            penalty,
            members: observed,
            member_file_ids: members.iter().map(|(fid, _)| *fid).collect(),
            basis,
        });
    }

    Ok(WorkspaceHealth { files, components })
}

/// A component's instability penalty input: known (possibly no instability
/// entry → no penalty), or unknown because the computation failed.
enum Penalty {
    Known(Option<ComponentInstability>),
    Unknown,
}

impl Penalty {
    /// The exact penalty, when known (no instability entry → no penalty).
    fn known(&self) -> Option<f64> {
        match self {
            Penalty::Known(i) => Some(
                i.as_ref()
                    .map_or(0.0, |i| scoring::instability_penalty(i.instability)),
            ),
            Penalty::Unknown => None,
        }
    }
}

/// Aggregate member scores into a component value and its basis digest.
fn score_members(
    members: &[(i64, i64)],
    by_id: &HashMap<i64, &ScoredFile<'_>>,
    instability: &Penalty,
    membership_current: bool,
) -> (ScoreValue, Digest) {
    let mut lower_pairs = Vec::with_capacity(members.len());
    let mut upper_pairs = Vec::with_capacity(members.len());
    let mut complete = true;
    let mut identity: Vec<(&str, String)> = Vec::with_capacity(members.len());
    for &(fid, nloc) in members {
        match by_id.get(&fid) {
            Some(f) => {
                lower_pairs.push((f.score.value.lower(), nloc));
                upper_pairs.push((f.score.value.upper(), nloc));
                complete &= f.score.value.is_measured();
                identity.push((&f.evidence.path, f.evidence.basis.to_hex()));
            }
            // A member with no evidence row is unobserved: full-range bounds.
            None => {
                lower_pairs.push((scoring::MIN_SCORE, nloc));
                upper_pairs.push((scoring::MAX_SCORE, nloc));
                complete = false;
            }
        }
    }
    // (least, most) penalty: exact when instability is known, else [0, max].
    let (min_penalty, max_penalty) = match instability.known() {
        Some(p) => (p, p),
        None => {
            complete = false;
            (0.0, scoring::instability_penalty(1.0))
        }
    };
    complete &= membership_current;
    let lower = scoring::component_score(&lower_pairs, max_penalty);
    let upper = scoring::component_score(&upper_pairs, min_penalty);
    let value = if complete {
        ScoreValue::Measured(upper)
    } else {
        ScoreValue::Partial { lower, upper }
    };

    identity.sort_unstable();
    let mut buf = format!(
        "{COMPONENT_SCORING_VERSION}\n{}\n",
        scoring::instability_penalty(1.0)
    );
    for (path, basis) in &identity {
        buf.push_str(&format!("{path}|{basis}\n"));
    }
    (value, Digest::of(buf.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::evidence::DeferReason;

    #[test]
    fn unknown_instability_widens_the_component_lower_bound_instead_of_failing() {
        let evidence = FileEvidence {
            path: "src/a.rs".into(),
            file_id: 1,
            outcomes: PERSISTENT_PRODUCERS
                .iter()
                .map(|&k| (k, ProducerOutcome::Complete { finding_count: 0 }))
                .collect(),
            findings: Vec::new(),
            waived: Vec::new(),
            basis: Digest::of(b"b"),
        };
        let scored = ScoredFile {
            evidence: &evidence,
            score: evidence.score(),
        };
        let by_id: HashMap<i64, &ScoredFile<'_>> = [(1, &scored)].into_iter().collect();
        let members = [(1, 100)];

        let (known, _) = score_members(&members, &by_id, &Penalty::Known(None), true);
        assert_eq!(known, ScoreValue::Measured(10.0));
        let (unknown, _) = score_members(&members, &by_id, &Penalty::Unknown, true);
        assert_eq!(
            unknown,
            ScoreValue::Partial {
                lower: 10.0 - scoring::instability_penalty(1.0),
                upper: 10.0
            }
        );
    }

    #[test]
    fn stale_validity_turns_complete_and_repo_unsupported_into_missing() {
        let stale = Validity::Stale(MissingReason::Deferred(DeferReason::LockBusy));
        assert_eq!(
            apply_validity(ProducerOutcome::Complete { finding_count: 2 }, stale),
            ProducerOutcome::Missing(MissingReason::Deferred(DeferReason::LockBusy))
        );
        assert_eq!(
            apply_validity(
                ProducerOutcome::Unsupported(UnsupportedReason::ConfirmedNonRepository),
                stale
            ),
            ProducerOutcome::Missing(MissingReason::Deferred(DeferReason::LockBusy))
        );
        assert_eq!(
            apply_validity(
                ProducerOutcome::Unsupported(UnsupportedReason::NoCoverageIngestion),
                stale
            ),
            ProducerOutcome::Unsupported(UnsupportedReason::NoCoverageIngestion)
        );
        assert_eq!(
            apply_validity(
                ProducerOutcome::Complete { finding_count: 0 },
                Validity::Current
            ),
            ProducerOutcome::Complete { finding_count: 0 }
        );
    }
}
