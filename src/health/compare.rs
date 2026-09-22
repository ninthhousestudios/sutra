//! Temporal comparison and on-demand attribution (sutra/416).
//!
//! Two separate questions, never conflated (health-evidence-contract.md §
//! Comparison and scoring):
//!
//! - **Temporal**: did the persistent health of a file change between two
//!   observations? Measured only when both sides are complete and share a
//!   scoring basis; otherwise explicitly incomparable with both observations
//!   preserved. [`temporal_blocker`] is the one rule, used by trend (snapshot vs
//!   snapshot) and review (checkpoint vs current run).
//! - **Attribution**: what do the fresh review-time findings (blame, shape diff)
//!   cost against the *same* current persistent evidence, under shared category
//!   caps? [`attribute`] — never a temporal delta, and never folded into one.

use std::collections::HashSet;

use crate::db::{Db, SnapshotCompleteness};
use crate::error::Result;
use crate::health::evidence::{HistoryObservation, HistoryStamp, InputStamp, RunId};
use crate::health::scoring::{
    EvidencePart, FileHealthScore, HealthCategory, MissingProducer, ScoreValue, scenario_score,
    score_file,
};

/// How a comparison selects its baseline checkpoint.
///
/// Review pins its baseline *before* `tool_context` can heal the index and
/// record a newer snapshot, and a genuinely-missing baseline (no checkpoint at
/// pin time) stays missing rather than silently comparing against this request's
/// own fresh snapshot (sutra/424 F5). `Latest` resolves at compute time.
#[derive(Debug, Clone, Copy)]
pub enum BaselineSelector {
    Latest,
    Pinned(Option<i64>),
}

impl BaselineSelector {
    pub fn resolve(&self, db: &Db) -> Result<Option<i64>> {
        match *self {
            BaselineSelector::Pinned(id) => Ok(id),
            BaselineSelector::Latest => Ok(db.latest_snapshots(1)?.first().map(|s| s.id)),
        }
    }
}

/// Why a temporal comparison is not a measured change. Stable wire tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncomparableReason {
    /// No baseline checkpoint existed (review).
    MissingBaseline,
    /// The file has no baseline observation.
    NewFile,
    /// The file has no current observation.
    RemovedFile,
    /// A side never recorded completeness (legacy row).
    UnknownCompleteness,
    /// A side is a partial observation (bounds, not a measurement).
    Partial,
    /// A side never recorded a scoring basis (legacy row).
    UnknownBasis,
    /// Both sides complete but scored under different rules (waiver policy,
    /// weights, applicability, scoring/producer version, membership).
    BasisChanged,
}

impl IncomparableReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            IncomparableReason::MissingBaseline => "missing_baseline",
            IncomparableReason::NewFile => "new_file",
            IncomparableReason::RemovedFile => "removed_file",
            IncomparableReason::UnknownCompleteness => "unknown_completeness",
            IncomparableReason::Partial => "partial",
            IncomparableReason::UnknownBasis => "unknown_basis",
            IncomparableReason::BasisChanged => "score_basis_changed",
        }
    }
}

/// What the temporal rule needs from one observation.
#[derive(Debug, Clone, Copy)]
pub struct SideSummary<'a> {
    pub completeness: SnapshotCompleteness,
    pub basis: Option<&'a str>,
}

impl<'a> SideSummary<'a> {
    /// Summary of a freshly scored current observation.
    pub fn of_score(score: &FileHealthScore, basis_hex: &'a str) -> Self {
        SideSummary {
            completeness: if score.value.is_measured() {
                SnapshotCompleteness::Complete
            } else {
                SnapshotCompleteness::Partial
            },
            basis: Some(basis_hex),
        }
    }
}

/// `None` when the two observations support a measured delta; otherwise the
/// first reason they do not. Checked in order: missing side, unknown
/// completeness, partial evidence, unknown basis, changed basis.
pub fn temporal_blocker(
    prev: Option<SideSummary<'_>>,
    cur: Option<SideSummary<'_>>,
) -> Option<IncomparableReason> {
    let (prev, cur) = match (prev, cur) {
        (Some(p), Some(c)) => (p, c),
        (None, Some(_)) | (None, None) => return Some(IncomparableReason::NewFile),
        (Some(_), None) => return Some(IncomparableReason::RemovedFile),
    };
    let sides = [prev.completeness, cur.completeness];
    if sides.contains(&SnapshotCompleteness::Unknown) {
        return Some(IncomparableReason::UnknownCompleteness);
    }
    if sides.contains(&SnapshotCompleteness::Partial) {
        return Some(IncomparableReason::Partial);
    }
    match (prev.basis, cur.basis) {
        (Some(a), Some(b)) if a == b => None,
        (Some(_), Some(_)) => Some(IncomparableReason::BasisChanged),
        _ => Some(IncomparableReason::UnknownBasis),
    }
}

/// Which input axes differ between the runs two observations were scored from —
/// the explanation the contract asks temporal results to carry ("explain
/// history/window/config changes"; a measured change does not by itself prove
/// the source edit caused it). Tokens, in axis order: `reindexed`,
/// `graph_rules` (parser/resolver identity), `graph`, `indexed_paths`,
/// `analysis_version`, `history_head`, `history_window`, `history_day`,
/// `history_state` (loaded/empty/unsupported/unknown changed), `owners_config`,
/// `rollups`.
pub fn input_changes(from: &InputStamp, to: &InputStamp) -> Vec<&'static str> {
    let mut out = Vec::new();
    if from.graph.epoch != to.graph.epoch {
        out.push("reindexed");
    }
    if from.graph.parser != to.graph.parser || from.graph.resolver != to.graph.resolver {
        out.push("graph_rules");
    }
    if from.graph.generation != to.graph.generation {
        out.push("graph");
    }
    if from.graph.indexed_paths != to.graph.indexed_paths {
        out.push("indexed_paths");
    }
    if from.analysis_version != to.analysis_version {
        out.push("analysis_version");
    }
    if let (Some(a), Some(b)) = (history_stamp(&from.history), history_stamp(&to.history)) {
        if a.repository != b.repository {
            out.push("history_head");
        }
        if a.window_days != b.window_days {
            out.push("history_window");
        }
        if a.day != b.day {
            out.push("history_day");
        }
    }
    if std::mem::discriminant(&from.history) != std::mem::discriminant(&to.history) {
        out.push("history_state");
    }
    if from.owners != to.owners {
        out.push("owners_config");
    }
    if from.rollups != to.rollups {
        out.push("rollups");
    }
    out
}

fn history_stamp(h: &HistoryObservation) -> Option<&HistoryStamp> {
    match h {
        HistoryObservation::Loaded(s) | HistoryObservation::Empty(s) => Some(s),
        _ => None,
    }
}

/// `input_changes` between two optional run ids, as JSON: `null` when either
/// side has no recorded run (legacy provenance) or its run is not retained.
pub fn input_changes_json(
    db: &Db,
    from: Option<i64>,
    to: Option<i64>,
) -> Result<serde_json::Value> {
    let (Some(a), Some(b)) = (from, to) else {
        return Ok(serde_json::Value::Null);
    };
    let (Some(ra), Some(rb)) = (db.load_health_run(RunId(a))?, db.load_health_run(RunId(b))?)
    else {
        return Ok(serde_json::Value::Null);
    };
    Ok(serde_json::json!(input_changes(&ra.inputs, &rb.inputs)))
}

/// The marginal score effect of on-demand evidence.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MarginalEffect {
    /// Both persistent and on-demand evidence complete: the exact effect. Can be
    /// zero despite a real finding when its category cap is already saturated.
    Exact(f64),
    /// Some producer is missing on either side: the effect lies in
    /// `[lower, upper]` (both ≤ 0). `upper` assumes missing persistent debt
    /// saturates its categories and missing on-demand producers found nothing;
    /// `lower` assumes the reverse.
    Conditional { lower: f64, upper: f64 },
}

/// Deduction attributed to one on-demand finding.
#[derive(Debug, Clone, Copy)]
pub struct FindingAttribution {
    /// Index into the on-demand part's findings.
    pub index: usize,
    pub raw_deduction: f64,
    /// Its share of the category's capped known deduction, with persistent
    /// findings competing for the same cap.
    pub scaled_deduction: f64,
}

#[derive(Debug)]
pub struct OnDemandAttribution {
    pub without: ScoreValue,
    pub with: ScoreValue,
    pub effect: MarginalEffect,
    pub findings: Vec<FindingAttribution>,
    /// On-demand producers that could not be evaluated for this file.
    pub missing: Vec<MissingProducer>,
}

/// Attribute fresh on-demand evidence against the current persistent evidence,
/// sharing the ordinary category caps (contract: "compares the same current
/// persistent evidence with and without the fresh review findings").
pub fn attribute(persistent: EvidencePart<'_>, ondemand: EvidencePart<'_>) -> OnDemandAttribution {
    let without = score_file(&[persistent]);
    let with = score_file(&[persistent, ondemand]);

    let p_missing = persistent.missing_categories();
    let d_missing = ondemand.missing_categories();
    let effect = if p_missing.is_empty() && d_missing.is_empty() {
        MarginalEffect::Exact(with.value.upper() - without.value.upper())
    } else {
        let both = [persistent, ondemand];
        // Least negative: unknown persistent debt saturates its categories (the
        // on-demand debt there costs nothing more); missing on-demand found none.
        let upper = scenario_score(&both, &p_missing) - scenario_score(&[persistent], &p_missing);
        // Most negative: no unknown persistent debt; missing on-demand producers
        // saturate their categories.
        let lower = scenario_score(&both, &d_missing)
            - scenario_score(&[persistent], &HashSet::<HealthCategory>::new());
        MarginalEffect::Conditional {
            lower: lower.min(upper),
            upper: upper.max(lower),
        }
    };

    let findings = with
        .deductions
        .iter()
        .filter(|d| d.part == 1)
        .map(|d| FindingAttribution {
            index: d.index,
            raw_deduction: d.raw_deduction,
            scaled_deduction: d.scaled_deduction,
        })
        .collect();
    let missing = score_file(&[ondemand]).missing;

    OnDemandAttribution {
        without: without.value,
        with: with.value,
        effect,
        findings,
        missing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::HealthFindingRow;
    use crate::health::evidence::{InputFailure, MissingReason, ProducerOutcome};
    use crate::health::findings::BiomarkerKind;
    use crate::health::scoring::{PERSISTENT_PRODUCERS, ProducerResult};

    fn row(id: i64, kind: BiomarkerKind) -> HealthFindingRow {
        HealthFindingRow {
            id,
            file_id: 1,
            symbol_id: None,
            biomarker_kind: kind.as_str().to_string(),
            severity: kind.default_severity().as_str().to_string(),
            confidence: 1.0,
            provenance: "test".into(),
            metric_value: 1.0,
            threshold: 1.0,
            detail: String::new(),
        }
    }

    fn complete() -> Vec<ProducerResult> {
        PERSISTENT_PRODUCERS
            .iter()
            .map(|&k| (k, ProducerOutcome::Complete { finding_count: 0 }))
            .collect()
    }

    fn blame_complete() -> Vec<ProducerResult> {
        vec![
            (
                BiomarkerKind::FunctionHotspot,
                ProducerOutcome::Complete { finding_count: 1 },
            ),
            (
                BiomarkerKind::CodeAgeVolatility,
                ProducerOutcome::Complete { finding_count: 0 },
            ),
        ]
    }

    fn side(c: SnapshotCompleteness, basis: Option<&str>) -> Option<SideSummary<'_>> {
        Some(SideSummary {
            completeness: c,
            basis,
        })
    }

    #[test]
    fn temporal_rule_orders_reasons() {
        use SnapshotCompleteness::*;
        assert_eq!(
            temporal_blocker(side(Complete, Some("a")), side(Complete, Some("a"))),
            None
        );
        assert_eq!(
            temporal_blocker(None, side(Complete, Some("a"))),
            Some(IncomparableReason::NewFile)
        );
        assert_eq!(
            temporal_blocker(side(Complete, Some("a")), None),
            Some(IncomparableReason::RemovedFile)
        );
        assert_eq!(
            temporal_blocker(side(Unknown, None), side(Complete, Some("a"))),
            Some(IncomparableReason::UnknownCompleteness)
        );
        assert_eq!(
            temporal_blocker(side(Complete, Some("a")), side(Partial, Some("a"))),
            Some(IncomparableReason::Partial)
        );
        assert_eq!(
            temporal_blocker(side(Complete, None), side(Complete, Some("a"))),
            Some(IncomparableReason::UnknownBasis)
        );
        assert_eq!(
            temporal_blocker(side(Complete, Some("a")), side(Complete, Some("b"))),
            Some(IncomparableReason::BasisChanged)
        );
    }

    #[test]
    fn exact_attribution_under_an_unsaturated_cap() {
        let p = complete();
        let d = blame_complete();
        let dfind = [row(-1, BiomarkerKind::FunctionHotspot)];
        let a = attribute(
            EvidencePart {
                outcomes: &p,
                findings: &[],
            },
            EvidencePart {
                outcomes: &d,
                findings: &dfind,
            },
        );
        match a.effect {
            MarginalEffect::Exact(e) => assert!((e + 1.16).abs() < 1e-9),
            other => panic!("expected exact, got {other:?}"),
        }
        assert_eq!(a.findings.len(), 1);
        assert!((a.findings[0].raw_deduction - 1.16).abs() < 1e-9);
    }

    #[test]
    fn saturated_cap_gives_zero_marginal_effect_despite_a_real_finding() {
        // Structural cap 2.5 already saturated by persistent debt
        // (nested 1.34 + blast 1.00 + nested 1.34 = 3.68).
        let p = complete();
        let pfind = [
            row(1, BiomarkerKind::NestedComplexity),
            row(2, BiomarkerKind::BlastRadiusChurn),
            row(3, BiomarkerKind::NestedComplexity),
        ];
        let d = blame_complete();
        let dfind = [row(-1, BiomarkerKind::FunctionHotspot)];
        let a = attribute(
            EvidencePart {
                outcomes: &p,
                findings: &pfind,
            },
            EvidencePart {
                outcomes: &d,
                findings: &dfind,
            },
        );
        assert_eq!(a.effect, MarginalEffect::Exact(0.0));
        let f = a.findings[0];
        assert!((f.raw_deduction - 1.16).abs() < 1e-9);
        assert!(f.scaled_deduction > 0.0 && f.scaled_deduction < 1.16);
    }

    #[test]
    fn missing_persistent_evidence_only_bounds_the_effect() {
        let mut p = complete();
        for (k, o) in p.iter_mut() {
            if *k == BiomarkerKind::BlastRadiusChurn {
                *o = ProducerOutcome::Missing(MissingReason::NoHistory);
            }
        }
        let d = blame_complete();
        let dfind = [row(-1, BiomarkerKind::FunctionHotspot)];
        let a = attribute(
            EvidencePart {
                outcomes: &p,
                findings: &[],
            },
            EvidencePart {
                outcomes: &d,
                findings: &dfind,
            },
        );
        match a.effect {
            MarginalEffect::Conditional { lower, upper } => {
                assert!((lower + 1.16).abs() < 1e-9, "lower {lower}");
                assert_eq!(upper, 0.0);
            }
            other => panic!("expected conditional, got {other:?}"),
        }
    }

    #[test]
    fn missing_on_demand_evidence_widens_the_lower_bound() {
        let p = complete();
        let d = vec![
            (
                BiomarkerKind::FunctionHotspot,
                ProducerOutcome::Missing(MissingReason::Failed(InputFailure::ProbeFailed)),
            ),
            (
                BiomarkerKind::CodeAgeVolatility,
                ProducerOutcome::Missing(MissingReason::Failed(InputFailure::ProbeFailed)),
            ),
        ];
        let a = attribute(
            EvidencePart {
                outcomes: &p,
                findings: &[],
            },
            EvidencePart {
                outcomes: &d,
                findings: &[],
            },
        );
        assert_eq!(a.missing.len(), 2);
        match a.effect {
            MarginalEffect::Conditional { lower, upper } => {
                // Structural 2.5 + freshness 1.5 saturated.
                assert!((lower + 4.0).abs() < 1e-9, "lower {lower}");
                assert_eq!(upper, 0.0);
            }
            other => panic!("expected conditional, got {other:?}"),
        }
    }
}
