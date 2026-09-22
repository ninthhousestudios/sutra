use std::collections::HashSet;

use crate::db::HealthFindingRow;
use crate::health::evidence::{Digest, MissingReason, ProducerOutcome, UnsupportedReason};
use crate::health::findings::{BiomarkerKind, HealthSeverity};

const BASE_SCORE: f64 = 10.0;
pub(crate) const MIN_SCORE: f64 = 1.0;
pub(crate) const MAX_SCORE: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HealthCategory {
    Organizational,
    Structural,
    Coupling,
    Freshness,
    Coverage,
}

impl HealthCategory {
    pub const ALL: [HealthCategory; 5] = [
        Self::Organizational,
        Self::Structural,
        Self::Coupling,
        Self::Freshness,
        Self::Coverage,
    ];

    pub fn cap(&self) -> f64 {
        match self {
            Self::Organizational => 3.5,
            Self::Structural => 2.5,
            Self::Coupling => 2.0,
            Self::Freshness => 1.5,
            Self::Coverage => 2.0,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Organizational => "organizational",
            Self::Structural => "structural",
            Self::Coupling => "coupling",
            Self::Freshness => "freshness",
            Self::Coverage => "coverage",
        }
    }
}

impl BiomarkerKind {
    pub fn category(&self) -> HealthCategory {
        match self {
            Self::CoChangeScatter | Self::ChangeEntropy | Self::OwnershipRisk => {
                HealthCategory::Organizational
            }
            Self::NestedComplexity | Self::FunctionHotspot | Self::BlastRadiusChurn => {
                HealthCategory::Structural
            }
            Self::HiddenCoupling | Self::ComponentInstability | Self::ImportCycle => {
                HealthCategory::Coupling
            }
            Self::CodeAgeVolatility | Self::HrrShapeChange => HealthCategory::Freshness,
            Self::DeadCodeRatio | Self::CoverageGradient => HealthCategory::Coverage,
        }
    }

    pub fn default_weight(&self) -> f64 {
        match self {
            // Repowise calibrated (13-repo corpus, T0 protocol)
            Self::CoChangeScatter => 1.80,
            Self::ChangeEntropy => 1.51,
            Self::OwnershipRisk => 1.38,
            Self::NestedComplexity => 1.34,
            Self::FunctionHotspot => 1.16,
            Self::CodeAgeVolatility => 1.10,
            // Non-repowise, moderate defaults
            Self::HiddenCoupling => 1.00,
            Self::BlastRadiusChurn => 1.00,
            Self::DeadCodeRatio => 0.80,
            Self::CoverageGradient => 0.80,
            // Sutra-specific, uncalibrated
            Self::ComponentInstability => 0.50,
            Self::HrrShapeChange => 0.50,
            Self::ImportCycle => 0.50,
        }
    }
}

impl HealthSeverity {
    pub fn weight(&self) -> f64 {
        match self {
            Self::Advisory => 1.0,
            Self::Informational => 0.5,
        }
    }
}

/// Where a biomarker is produced and therefore which observation scores it.
/// Single exhaustive source of truth: a new variant forces a decision here, so a
/// biomarker can never silently drop out of the "missing analysis is never zero
/// debt" contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiomarkerScope {
    /// Parse-time, per-file, persisted in a health run with an explicit outcome
    /// per file (`health_runs`). Temporal comparison uses only these.
    Persistent,
    /// Computed fresh at review time (blame, shape diff); never persisted, never
    /// part of a temporal delta — only of on-demand attribution.
    OnDemand,
    /// Component-scoped (instability), applied to component scores only.
    Component,
}

impl BiomarkerKind {
    pub fn scope(&self) -> BiomarkerScope {
        match self {
            Self::NestedComplexity
            | Self::CoChangeScatter
            | Self::ChangeEntropy
            | Self::OwnershipRisk
            | Self::HiddenCoupling
            | Self::BlastRadiusChurn
            | Self::DeadCodeRatio
            | Self::ImportCycle
            | Self::CoverageGradient => BiomarkerScope::Persistent,
            Self::FunctionHotspot | Self::CodeAgeVolatility | Self::HrrShapeChange => {
                BiomarkerScope::OnDemand
            }
            Self::ComponentInstability => BiomarkerScope::Component,
        }
    }
}

/// The persistent per-file producers, in canonical order. Every health run stages
/// exactly one outcome per (file, producer) in this list; readers treat a
/// producer without an outcome as missing. Must equal the `Persistent` scope
/// (unit-tested).
pub const PERSISTENT_PRODUCERS: [BiomarkerKind; 9] = [
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

/// Scoring-algorithm identity folded into every [`ScoreBasis`]. Bump when the
/// scoring *rules* change (bound semantics, clamping, which findings count);
/// weights, severities and caps are digested directly so they need no bump.
pub const SCORING_VERSION: &str = "health-scoring-v2-interval";

impl UnsupportedReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnsupportedReason::ConfirmedNonRepository => "not a git repository",
            UnsupportedReason::NoCoverageIngestion => "no coverage data source",
        }
    }
}

/// One producer's outcome for one file, as consumed by scoring.
pub type ProducerResult = (BiomarkerKind, ProducerOutcome);

/// A slice of validated evidence for one file: the producers it observed and
/// their findings. Scoring takes several parts so the persistent run and fresh
/// on-demand evidence share category caps without being merged (and without a
/// finding from one part being authorized by an outcome from the other — each
/// finding counts only when *its own part* recorded its producer `Complete`).
#[derive(Debug, Clone, Copy)]
pub struct EvidencePart<'a> {
    pub outcomes: &'a [ProducerResult],
    /// Active (non-waived) findings. A finding whose producer is not `Complete`
    /// in this part is stale or unauthorized and never counts as known debt.
    pub findings: &'a [HealthFindingRow],
}

impl EvidencePart<'_> {
    fn outcome(&self, kind: BiomarkerKind) -> Option<&ProducerOutcome> {
        self.outcomes
            .iter()
            .find(|(k, _)| *k == kind)
            .map(|(_, o)| o)
    }

    /// Categories in which this part has an applicable producer without current
    /// evidence — the categories its pessimistic bound saturates.
    pub fn missing_categories(&self) -> HashSet<HealthCategory> {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, ProducerOutcome::Missing(_)))
            .map(|(k, _)| k.category())
            .collect()
    }
}

/// A file score. `Measured` only when every applicable producer in every part
/// is `Complete`; otherwise an interval — never a point value dressed up as a
/// measurement (health-evidence-contract.md § Comparison and scoring).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScoreValue {
    Measured(f64),
    /// `upper` counts only known current debt; `lower` saturates every category
    /// with a missing producer at its cap. A conservative bound, not a proved
    /// worst case of any particular finding set.
    Partial {
        lower: f64,
        upper: f64,
    },
}

impl ScoreValue {
    pub fn lower(&self) -> f64 {
        match *self {
            ScoreValue::Measured(s) => s,
            ScoreValue::Partial { lower, .. } => lower,
        }
    }

    pub fn upper(&self) -> f64 {
        match *self {
            ScoreValue::Measured(s) => s,
            ScoreValue::Partial { upper, .. } => upper,
        }
    }

    pub fn is_measured(&self) -> bool {
        matches!(self, ScoreValue::Measured(_))
    }
}

#[derive(Debug)]
pub struct FindingDeduction {
    /// Which [`EvidencePart`] and which finding within it.
    pub part: usize,
    pub index: usize,
    pub finding_id: i64,
    pub raw_deduction: f64,
    /// Share of the category's *known* (capped) deduction.
    pub scaled_deduction: f64,
    pub category: HealthCategory,
}

/// Per-category deduction bounds.
#[derive(Debug, Clone, Copy)]
pub struct CategoryDeduction {
    pub category: HealthCategory,
    /// Sum of raw known-finding deductions before capping.
    pub known_raw: f64,
    /// Known debt, capped: the optimistic deduction.
    pub known: f64,
    /// The cap when a producer in this category is missing, else `known`.
    pub pessimistic: f64,
}

/// A producer with no current evidence for the file.
#[derive(Debug, Clone, Copy)]
pub struct MissingProducer {
    pub biomarker: BiomarkerKind,
    pub reason: MissingReason,
}

#[derive(Debug)]
pub struct FileHealthScore {
    pub value: ScoreValue,
    pub deductions: Vec<FindingDeduction>,
    /// Every category with known debt or a missing producer, in
    /// [`HealthCategory::ALL`] order.
    pub categories: Vec<CategoryDeduction>,
    pub missing: Vec<MissingProducer>,
    /// Structurally inapplicable producers, excluded from the score.
    pub unsupported: Vec<(BiomarkerKind, UnsupportedReason)>,
}

impl FileHealthScore {
    pub fn partial(&self) -> bool {
        !self.value.is_measured()
    }

    /// Missing biomarker names, sorted and deduplicated — the persisted
    /// `missing_biomarkers` vocabulary.
    pub fn missing_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .missing
            .iter()
            .map(|m| m.biomarker.as_str().to_string())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }
}

/// Known raw deductions per category: every finding whose producer its own part
/// recorded `Complete`. Returns `(part, index, category, raw)`.
fn known_findings(parts: &[EvidencePart<'_>]) -> Vec<(usize, usize, HealthCategory, f64)> {
    let mut known = Vec::new();
    for (p, part) in parts.iter().enumerate() {
        for (i, f) in part.findings.iter().enumerate() {
            let Some(kind) = BiomarkerKind::parse(&f.biomarker_kind) else {
                continue;
            };
            if !matches!(part.outcome(kind), Some(ProducerOutcome::Complete { .. })) {
                continue;
            }
            let Some(severity) = HealthSeverity::parse(&f.severity) else {
                continue;
            };
            known.push((
                p,
                i,
                kind.category(),
                severity.weight() * kind.default_weight(),
            ));
        }
    }
    known
}

/// Score under one scenario: every category in `saturate` deducts its full cap;
/// the rest deduct their capped known debt. The shared primitive behind the
/// score bounds and the on-demand attribution bounds, so both apply the same
/// category caps and global clamp.
pub fn scenario_score(parts: &[EvidencePart<'_>], saturate: &HashSet<HealthCategory>) -> f64 {
    let known = known_findings(parts);
    let total: f64 = HealthCategory::ALL
        .iter()
        .map(|&cat| {
            if saturate.contains(&cat) {
                cat.cap()
            } else {
                let raw: f64 = known
                    .iter()
                    .filter(|(_, _, c, _)| *c == cat)
                    .map(|(_, _, _, r)| r)
                    .sum();
                raw.min(cat.cap())
            }
        })
        .sum();
    clamp_score(BASE_SCORE - total)
}

fn clamp_score(s: f64) -> f64 {
    s.clamp(MIN_SCORE, MAX_SCORE)
}

/// Score one file from validated evidence (health-evidence-contract.md §
/// Comparison and scoring). Findings count only when their producer is
/// `Complete` in their own part — stale findings never become known debt. A
/// `Missing` producer saturates its category in the lower bound; `Unsupported`
/// producers are excluded and reported. A producer with no outcome in any part
/// was not observed by this caller and is not part of the score.
pub fn score_file(parts: &[EvidencePart<'_>]) -> FileHealthScore {
    let known = known_findings(parts);

    let mut missing = Vec::new();
    let mut unsupported = Vec::new();
    for part in parts {
        for &(kind, outcome) in part.outcomes {
            match outcome {
                ProducerOutcome::Complete { .. } => {}
                ProducerOutcome::Missing(reason) => missing.push(MissingProducer {
                    biomarker: kind,
                    reason,
                }),
                ProducerOutcome::Unsupported(reason) => unsupported.push((kind, reason)),
            }
        }
    }
    let saturated: HashSet<HealthCategory> =
        missing.iter().map(|m| m.biomarker.category()).collect();

    let mut deductions = Vec::new();
    let mut categories = Vec::new();
    let mut optimistic = 0.0;
    let mut pessimistic = 0.0;
    for cat in HealthCategory::ALL {
        let items: Vec<&(usize, usize, HealthCategory, f64)> =
            known.iter().filter(|(_, _, c, _)| *c == cat).collect();
        let known_raw: f64 = items.iter().map(|(_, _, _, r)| r).sum();
        let scale = if known_raw > cat.cap() {
            cat.cap() / known_raw
        } else {
            1.0
        };
        for &&(part, index, category, raw) in &items {
            deductions.push(FindingDeduction {
                part,
                index,
                finding_id: parts[part].findings[index].id,
                raw_deduction: raw,
                scaled_deduction: raw * scale,
                category,
            });
        }
        let known_capped = known_raw.min(cat.cap());
        let is_saturated = saturated.contains(&cat);
        let pess = if is_saturated {
            cat.cap()
        } else {
            known_capped
        };
        optimistic += known_capped;
        pessimistic += pess;
        if known_raw > 0.0 || is_saturated {
            categories.push(CategoryDeduction {
                category: cat,
                known_raw,
                known: known_capped,
                pessimistic: pess,
            });
        }
    }

    let upper = clamp_score(BASE_SCORE - optimistic);
    let value = if missing.is_empty() {
        ScoreValue::Measured(upper)
    } else {
        ScoreValue::Partial {
            lower: clamp_score(BASE_SCORE - pessimistic),
            upper,
        }
    };
    FileHealthScore {
        value,
        deductions,
        categories,
        missing,
        unsupported,
    }
}

/// The scoring basis of a persistent file observation: the identity two
/// observations must share before a temporal delta between them is a measured
/// change rather than a change of rules (health-evidence-contract.md §
/// Comparison and scoring). Covers the scoring version, producer/analysis
/// version, every producer's weight/severity/category/cap, which producers are
/// applicable to this file, and the waiver policy that applies to it. Input
/// generations and history windows are deliberately *not* part of it — those
/// move between temporal observations by design.
pub fn file_score_basis(
    outcomes: &[ProducerResult],
    waivers: &[&crate::db::HealthWaiverRow],
) -> Digest {
    let mut buf = String::new();
    buf.push_str(SCORING_VERSION);
    buf.push('\n');
    buf.push_str(crate::health::probe::HEALTH_ANALYSIS_VERSION);
    buf.push('\n');
    for kind in PERSISTENT_PRODUCERS {
        let applicability = match outcomes.iter().find(|(k, _)| *k == kind) {
            Some((_, ProducerOutcome::Unsupported(r))) => r.as_str(),
            _ => "applicable",
        };
        let cat = kind.category();
        buf.push_str(&format!(
            "{}|{}|{}|{}|{}|{}\n",
            kind.as_str(),
            kind.default_severity().weight(),
            kind.default_weight(),
            cat.as_str(),
            cat.cap(),
            applicability,
        ));
    }
    let mut policy: Vec<(&str, &str)> = waivers
        .iter()
        .map(|w| {
            (
                w.biomarker_kind.as_str(),
                w.symbol_qualified_name.as_deref().unwrap_or(""),
            )
        })
        .collect();
    policy.sort_unstable();
    policy.dedup();
    for (kind, symbol) in policy {
        buf.push_str(&format!("waiver|{kind}|{symbol}\n"));
    }
    Digest::of(buf.as_bytes())
}

pub fn score_component(file_scores: &[(f64, i64)]) -> f64 {
    let total_nloc: i64 = file_scores.iter().map(|(_, nloc)| *nloc).sum();
    if total_nloc == 0 {
        return MAX_SCORE;
    }
    let weighted_sum: f64 = file_scores
        .iter()
        .map(|(score, nloc)| score * (*nloc as f64))
        .sum();
    (weighted_sum / total_nloc as f64).clamp(MIN_SCORE, MAX_SCORE)
}

pub fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Component-level instability penalty applied to a component's NLOC-weighted
/// score. Instability (Martin's I = Ce/(Ca+Ce), 0..1) is a crude fragility
/// proxy: higher I means the component depends outward on more than depends on
/// it. The deduction follows the finding formula (Informational severity × the
/// ComponentInstability weight) scaled by I, capped at the coupling cap. The
/// weight is deliberately low (uncalibrated); a future repowise pass can raise
/// it. Applied identically in the snapshot and file_health paths so component
/// scores never diverge between them.
pub fn instability_penalty(instability: f64) -> f64 {
    let raw = HealthSeverity::Informational.weight()
        * BiomarkerKind::ComponentInstability.default_weight()
        * instability.clamp(0.0, 1.0);
    raw.min(HealthCategory::Coupling.cap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::evidence::InputFailure;

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

    fn complete_all() -> Vec<ProducerResult> {
        PERSISTENT_PRODUCERS
            .iter()
            .map(|&k| {
                let o = if k == BiomarkerKind::CoverageGradient {
                    ProducerOutcome::Unsupported(UnsupportedReason::NoCoverageIngestion)
                } else {
                    ProducerOutcome::Complete { finding_count: 0 }
                };
                (k, o)
            })
            .collect()
    }

    fn set(outcomes: &mut [ProducerResult], kind: BiomarkerKind, o: ProducerOutcome) {
        for (k, out) in outcomes.iter_mut() {
            if *k == kind {
                *out = o;
            }
        }
    }

    #[test]
    fn persistent_producers_match_scope() {
        let mut scoped: Vec<&str> = BiomarkerKind::ALL
            .into_iter()
            .filter(|k| k.scope() == BiomarkerScope::Persistent)
            .map(|k| k.as_str())
            .collect();
        let mut listed: Vec<&str> = PERSISTENT_PRODUCERS.iter().map(|k| k.as_str()).collect();
        scoped.sort_unstable();
        listed.sort_unstable();
        assert_eq!(scoped, listed);
    }

    #[test]
    fn complete_clean_file_is_measured_ten() {
        let outcomes = complete_all();
        let s = score_file(&[EvidencePart {
            outcomes: &outcomes,
            findings: &[],
        }]);
        assert_eq!(s.value, ScoreValue::Measured(10.0));
        assert_eq!(s.unsupported.len(), 1);
    }

    #[test]
    fn missing_producer_saturates_its_category_in_the_lower_bound() {
        let mut outcomes = complete_all();
        set(
            &mut outcomes,
            BiomarkerKind::ChangeEntropy,
            ProducerOutcome::Missing(MissingReason::NoHistory),
        );
        let s = score_file(&[EvidencePart {
            outcomes: &outcomes,
            findings: &[],
        }]);
        // Organizational cap 3.5 — not one finding's weight (1.51).
        assert_eq!(
            s.value,
            ScoreValue::Partial {
                lower: 6.5,
                upper: 10.0
            }
        );
    }

    #[test]
    fn findings_of_a_missing_producer_never_count_as_known_debt() {
        let mut outcomes = complete_all();
        set(
            &mut outcomes,
            BiomarkerKind::NestedComplexity,
            ProducerOutcome::Missing(MissingReason::InputsChanged),
        );
        let findings = [row(1, BiomarkerKind::NestedComplexity)];
        let s = score_file(&[EvidencePart {
            outcomes: &outcomes,
            findings: &findings,
        }]);
        assert!(
            s.deductions.is_empty(),
            "a stale finding is not current debt"
        );
        assert_eq!(s.value.upper(), 10.0);
        assert_eq!(s.value.lower(), 7.5);
    }

    #[test]
    fn a_part_cannot_authorize_another_parts_findings() {
        // On-demand findings with no on-demand outcome are unobserved, even though
        // the persistent part is complete.
        let outcomes = complete_all();
        let ondemand = [row(-1, BiomarkerKind::FunctionHotspot)];
        let s = score_file(&[
            EvidencePart {
                outcomes: &outcomes,
                findings: &[],
            },
            EvidencePart {
                outcomes: &[],
                findings: &ondemand,
            },
        ]);
        assert_eq!(s.value, ScoreValue::Measured(10.0));
    }

    #[test]
    fn failed_probe_is_missing_with_its_reason() {
        let mut outcomes = complete_all();
        set(
            &mut outcomes,
            BiomarkerKind::OwnershipRisk,
            ProducerOutcome::Missing(MissingReason::Failed(InputFailure::ConfigInvalid)),
        );
        let s = score_file(&[EvidencePart {
            outcomes: &outcomes,
            findings: &[],
        }]);
        assert_eq!(s.missing.len(), 1);
        assert_eq!(s.missing_names(), vec!["ownership_risk".to_string()]);
    }

    #[test]
    fn known_debt_is_capped_and_scaled() {
        let outcomes = complete_all();
        let findings = [
            row(1, BiomarkerKind::CoChangeScatter),
            row(2, BiomarkerKind::ChangeEntropy),
            row(3, BiomarkerKind::OwnershipRisk),
        ];
        let s = score_file(&[EvidencePart {
            outcomes: &outcomes,
            findings: &findings,
        }]);
        // 1.80 + 1.51 + 1.38 = 4.69 raw, capped at 3.5.
        assert!((s.value.upper() - 6.5).abs() < 1e-9);
        let scaled: f64 = s.deductions.iter().map(|d| d.scaled_deduction).sum();
        assert!((scaled - 3.5).abs() < 1e-9);
    }

    #[test]
    fn basis_changes_with_waiver_policy_and_applicability() {
        let outcomes = complete_all();
        let base = file_score_basis(&outcomes, &[]);
        let waiver = crate::db::HealthWaiverRow {
            id: 1,
            biomarker_kind: "nested_complexity".into(),
            file_path: "a.rs".into(),
            symbol_qualified_name: None,
            rationale: "r".into(),
            waived_by: "w".into(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        assert_ne!(base, file_score_basis(&outcomes, &[&waiver]));
        let mut no_repo = complete_all();
        set(
            &mut no_repo,
            BiomarkerKind::ChangeEntropy,
            ProducerOutcome::Unsupported(UnsupportedReason::ConfirmedNonRepository),
        );
        assert_ne!(base, file_score_basis(&no_repo, &[]));
        // A missing producer is still applicable: same basis as complete.
        let mut missing = complete_all();
        set(
            &mut missing,
            BiomarkerKind::ChangeEntropy,
            ProducerOutcome::Missing(MissingReason::NoHistory),
        );
        assert_eq!(base, file_score_basis(&missing, &[]));
    }
}
