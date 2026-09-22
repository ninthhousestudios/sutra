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
//! The caller supplies the run's [`Validity`] — established by the demand refresh
//! it just performed, or by [`crate::health::refresh::current_run_validity`]. A
//! stale run is still loaded (its findings stay visible) but every outcome that
//! depends on the moved inputs becomes `Missing`, so stale findings can never be
//! counted as known current debt.

use std::collections::HashMap;

use crate::db::{Db, HealthFindingRow, HealthWaiverRow};
use crate::error::Result;
use crate::health::evidence::{
    Digest, MissingReason, ProducerOutcome, RunId, UnsupportedReason, Validity,
};
use crate::health::findings::BiomarkerKind;
use crate::health::instability::{self, ComponentInstability};
use crate::health::scoring::{
    self, EvidencePart, FileHealthScore, PERSISTENT_PRODUCERS, ProducerResult, ScoreValue,
};
use crate::waivers::{self, ResolvedHealthFinding};

/// Component-aggregation identity folded into every component basis. Bump when
/// the aggregation rule (NLOC weighting, instability penalty form) changes.
const COMPONENT_SCORING_VERSION: &str = "component-scoring-v1-nloc-instability";

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
    pub fn load(db: &Db, validity: Validity) -> Result<Self> {
        let run = db.load_current_health_run()?;
        let waivers = db.get_health_waivers()?;
        let indexed = db.all_files()?;

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
            .map(|f| ResolvedHealthFinding {
                finding: f.finding,
                file_path: f.file_path,
                symbol_name: f.symbol_label,
            })
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
            let outcomes: Vec<ProducerResult> = PERSISTENT_PRODUCERS
                .iter()
                .map(|&kind| {
                    let stored = recorded
                        .iter()
                        .find(|(k, _)| *k == kind)
                        .map(|(_, o)| *o)
                        .unwrap_or(ProducerOutcome::Missing(absent));
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
                findings: active_by_path.remove(path).unwrap_or_default(),
                waived: waived_by_path.remove(path).unwrap_or_default(),
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
pub struct ScoredComponent {
    pub component_id: String,
    pub component_name: String,
    /// NLOC-weighted member scores minus the instability penalty. `Measured` only
    /// when every member file is measured.
    pub value: ScoreValue,
    pub member_count: usize,
    pub total_nloc: i64,
    pub instability: Option<ComponentInstability>,
    /// Membership + member bases + aggregation identity: two component
    /// observations compare as measured only when this matches.
    pub basis: Digest,
}

#[derive(Debug)]
pub struct WorkspaceHealth<'e> {
    pub files: Vec<ScoredFile<'e>>,
    pub components: Vec<ScoredComponent>,
}

/// Score every indexed file and every live component from validated evidence.
pub fn score_workspace<'e>(
    db: &Db,
    evidence: &'e PersistentEvidence,
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
    // Instability feeds the component score, so a failure is surfaced rather
    // than silently dropping the penalty.
    let mut instability_map = instability::compute_component_instability(db)?;

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
        let instability = instability_map.remove(&comp.id);
        let (value, basis) = score_members(members, &by_id, instability.as_ref());
        components.push(ScoredComponent {
            component_id: comp.id,
            component_name: comp.name,
            value,
            member_count: members.len(),
            total_nloc: members.iter().map(|(_, n)| n).sum(),
            instability,
            basis,
        });
    }

    Ok(WorkspaceHealth { files, components })
}

/// Aggregate member scores into a component value and its basis digest.
fn score_members(
    members: &[(i64, i64)],
    by_id: &HashMap<i64, &ScoredFile<'_>>,
    instability: Option<&ComponentInstability>,
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
    let penalty = instability.map_or(0.0, |i| scoring::instability_penalty(i.instability));
    let adjust = |base: f64| (base - penalty).clamp(scoring::MIN_SCORE, scoring::MAX_SCORE);
    let lower = adjust(scoring::score_component(&lower_pairs));
    let upper = adjust(scoring::score_component(&upper_pairs));
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
