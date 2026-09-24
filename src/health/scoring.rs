use std::collections::HashSet;

use crate::db::HealthFindingRow;
use crate::health::evidence::{Digest, MissingReason, ProducerOutcome, UnsupportedReason};
use crate::health::findings::{BiomarkerKind, HealthSeverity};

const BASE_SCORE: f64 = 10.0;
pub(crate) const MIN_SCORE: f64 = 1.0;
pub(crate) const MAX_SCORE: f64 = 10.0;

/// Category curve scale as a fraction of the category cap (sutra/404).
/// Provisional: 0.5 chosen on sutra's own index (145 files) — keeps a single
/// finding within ~20% of its weight while still separating the coupling tail.
/// Digested into [`file_score_basis`], so a change needs no version bump.
pub(crate) const CATEGORY_SCALE_FRACTION: f64 = 0.5;

/// Share of a component's deduction taken by the density term; the count term
/// takes the rest (sutra/404, trellis 50/50).
pub(crate) const COMPONENT_DENSITY_SHARE: f64 = 0.5;

/// Count-term curve for components: total member debt mass (Σ of member
/// deductions, NLOC-independent) → deduction, bounded by the full score range.
/// Scale provisional: 5.0 makes a single-member component with typical debt
/// (~2.7) score about as its member does.
pub(crate) const COMPONENT_COUNT: Saturation = Saturation::new(MAX_SCORE - MIN_SCORE, 5.0);

/// A bounded, strictly increasing soft-saturation (sutra/404):
/// `limit * b / (1 + b)` with `b = ln(1 + raw / scale)`. Zero at zero, strictly
/// increasing, never reaches `limit` — every extra unit of debt costs
/// something, yet `limit` stays a sound supremum for pessimistic bounds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Saturation {
    limit: f64,
    scale: f64,
}

impl Saturation {
    /// Const-only construction; a non-positive limit or scale is a
    /// compile-time error, never a runtime state.
    pub(crate) const fn new(limit: f64, scale: f64) -> Self {
        assert!(
            limit > 0.0 && scale > 0.0,
            "invariant: saturation limit and scale are positive"
        );
        Self { limit, scale }
    }

    pub fn limit(&self) -> f64 {
        self.limit
    }

    /// Deduction for `raw` debt; non-positive `raw` deducts nothing.
    pub fn apply(&self, raw: f64) -> f64 {
        let burden = (raw.max(0.0) / self.scale).ln_1p();
        self.limit * burden / (1.0 + burden)
    }
}

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

    /// The supremum of this category's deduction: known debt approaches it
    /// without reaching it, and a missing producer saturates to it in the
    /// pessimistic bound.
    pub fn cap(&self) -> f64 {
        match self {
            Self::Organizational => 3.5,
            Self::Structural => 2.5,
            Self::Coupling => 2.0,
            Self::Freshness => 1.5,
            Self::Coverage => 2.0,
        }
    }

    /// The curve mapping this category's raw known debt to its deduction:
    /// bounded by [`cap`](Self::cap), scaled at `cap * CATEGORY_SCALE_FRACTION`.
    pub fn saturation(&self) -> Saturation {
        Saturation::new(self.cap(), self.cap() * CATEGORY_SCALE_FRACTION)
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
/// scoring *rules* change (bound semantics, clamping, which findings count,
/// the saturation curve's form); biomarker weights, default severities, every
/// severity weight, category caps and the curve scale are digested directly so
/// they need no bump. The snapshot workspace score ([`workspace_score`]) has no
/// basis of its own: a change to its rule must bump this too.
pub const SCORING_VERSION: &str = "health-scoring-v3-log-saturation";

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
/// on-demand evidence share category saturation without being merged (and without a
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
    /// Proportional share of the category's *known* (saturated) deduction.
    /// Descriptive only: non-monotone — a sibling appearing shrinks it.
    pub scaled_deduction: f64,
    /// Exact rise of the optimistic (upper) score if this finding alone were
    /// resolved, all else fixed — including the global clamp, so it can be
    /// zero at the score floor. The actionable "fix this first" number.
    pub marginal: f64,
    pub category: HealthCategory,
}

/// Per-category deduction bounds.
#[derive(Debug, Clone, Copy)]
pub struct CategoryDeduction {
    pub category: HealthCategory,
    /// Sum of raw known-finding deductions before capping.
    pub known_raw: f64,
    /// Known debt, saturated: the optimistic deduction.
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
/// the rest deduct their saturated known debt. The shared primitive behind the
/// score bounds and the on-demand attribution bounds, so both apply the same
/// category saturation and global clamp.
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
                cat.saturation().apply(raw)
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

    let mut categories = Vec::new();
    let mut optimistic = 0.0;
    let mut pessimistic = 0.0;
    for cat in HealthCategory::ALL {
        let known_raw: f64 = known
            .iter()
            .filter(|(_, _, c, _)| *c == cat)
            .map(|(_, _, _, r)| r)
            .sum();
        let known_saturated = cat.saturation().apply(known_raw);
        let is_saturated = saturated.contains(&cat);
        let pess = if is_saturated {
            cat.cap()
        } else {
            known_saturated
        };
        optimistic += known_saturated;
        pessimistic += pess;
        if known_raw > 0.0 || is_saturated {
            categories.push(CategoryDeduction {
                category: cat,
                known_raw,
                known: known_saturated,
                pessimistic: pess,
            });
        }
    }

    let upper = clamp_score(BASE_SCORE - optimistic);
    // Grouped in category order, as consumers have always received them.
    let deductions = categories
        .iter()
        .flat_map(|cat| {
            known
                .iter()
                .filter(move |(_, _, c, _)| *c == cat.category)
                .map(move |f| (cat, f))
        })
        .map(|(cat, &(part, index, category, raw))| {
            let scale = cat.known / cat.known_raw;
            let without = optimistic - cat.known + category.saturation().apply(cat.known_raw - raw);
            FindingDeduction {
                part,
                index,
                finding_id: parts[part].findings[index].id,
                raw_deduction: raw,
                scaled_deduction: raw * scale,
                marginal: clamp_score(BASE_SCORE - without) - upper,
                category,
            }
        })
        .collect();

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
    buf.push_str(&format!(
        "category_scale_fraction|{CATEGORY_SCALE_FRACTION}\n"
    ));
    // Every severity weight, not just the defaults: a finding is weighted by its
    // recorded severity, which some producers escalate (hidden_coupling).
    for sev in [HealthSeverity::Advisory, HealthSeverity::Informational] {
        buf.push_str(&format!("severity|{}|{}\n", sev.as_str(), sev.weight()));
    }
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

/// A component's score: `MAX_SCORE` less a blended deduction —
/// [`COMPONENT_DENSITY_SHARE`] of the NLOC-weighted mean member deduction plus
/// the rest from [`COMPONENT_COUNT`] over total member debt mass — less the
/// instability `penalty`, clamped. The one aggregation rule, shared by the
/// scorer and trend's re-evaluation at baseline weights (sutra/436) so the two
/// cannot drift. Non-increasing in every member deduction and in `penalty`
/// (I2), which is what makes lower/upper member bounds yield sound component
/// bounds (I4); the count term survives any amount of clean NLOC (I3).
pub fn component_score(members: &[(f64, i64)], penalty: f64) -> f64 {
    let deduction = COMPONENT_DENSITY_SHARE * density_deduction(members)
        + (1.0 - COMPONENT_DENSITY_SHARE) * COMPONENT_COUNT.apply(debt_mass(members));
    (MAX_SCORE - deduction - penalty).clamp(MIN_SCORE, MAX_SCORE)
}

/// NLOC-weighted mean member deduction (`MAX_SCORE - score`); 0 when the
/// members carry no NLOC.
fn density_deduction(members: &[(f64, i64)]) -> f64 {
    let total_nloc: i64 = members.iter().map(|(_, nloc)| *nloc).sum();
    if total_nloc == 0 {
        return 0.0;
    }
    let weighted: f64 = members
        .iter()
        .map(|(score, nloc)| (MAX_SCORE - score) * (*nloc as f64))
        .sum();
    weighted / total_nloc as f64
}

/// Total member deduction, NLOC-independent: the absolute debt that clean
/// additions cannot dilute.
fn debt_mass(members: &[(f64, i64)]) -> f64 {
    members.iter().map(|(score, _)| MAX_SCORE - score).sum()
}

/// The workspace score: every file as one component with no instability
/// penalty — the same aggregation rule, so the workspace number stops being a
/// dilutable unweighted file mean (pipeline snapshot `health_score`).
pub fn workspace_score(files: &[(f64, i64)]) -> f64 {
    component_score(files, 0.0)
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
    fn known_debt_is_saturated_and_attributed() {
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
        // 1.80 + 1.51 + 1.38 = 4.69 raw: saturated below the 3.5 cap, and the
        // proportional shares add back up to the category deduction.
        let known = HealthCategory::Organizational.saturation().apply(4.69);
        assert!(known < 3.5);
        assert!((s.value.upper() - (10.0 - known)).abs() < 1e-9);
        let scaled: f64 = s.deductions.iter().map(|d| d.scaled_deduction).sum();
        assert!((scaled - known).abs() < 1e-9);
        // Each finding's marginal is the exact gain of resolving it alone.
        let without_scatter = HealthCategory::Organizational
            .saturation()
            .apply(4.69 - 1.80);
        assert!((s.deductions[0].marginal - (known - without_scatter)).abs() < 1e-9);
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

/// The monotonicity invariants committed to in sutra/404, as properties.
#[cfg(test)]
mod invariants {
    use proptest::prelude::*;

    use super::*;

    fn all_complete() -> Vec<ProducerResult> {
        PERSISTENT_PRODUCERS
            .iter()
            .map(|&k| (k, ProducerOutcome::Complete { finding_count: 0 }))
            .collect()
    }

    fn finding(id: i64, (producer, advisory): (usize, bool)) -> HealthFindingRow {
        let severity = if advisory {
            HealthSeverity::Advisory
        } else {
            HealthSeverity::Informational
        };
        HealthFindingRow {
            id,
            file_id: 1,
            symbol_id: None,
            biomarker_kind: PERSISTENT_PRODUCERS[producer].as_str().to_string(),
            severity: severity.as_str().to_string(),
            confidence: 1.0,
            provenance: "prop".into(),
            metric_value: 1.0,
            threshold: 1.0,
            detail: String::new(),
        }
    }

    fn arb_finding() -> impl Strategy<Value = (usize, bool)> {
        (0..PERSISTENT_PRODUCERS.len(), any::<bool>())
    }

    fn arb_members() -> impl Strategy<Value = Vec<(f64, i64)>> {
        prop::collection::vec((MIN_SCORE..=MAX_SCORE, 0_i64..5_000), 0..12)
    }

    fn upper(findings: &[HealthFindingRow]) -> FileHealthScore {
        let outcomes = all_complete();
        score_file(&[EvidencePart {
            outcomes: &outcomes,
            findings,
        }])
    }

    proptest! {
        #[test]
        fn saturation_is_zero_at_zero_strictly_increasing_and_bounded(
            a in 0.0_f64..1e6,
            b in 0.0_f64..1e6,
        ) {
            for cat in HealthCategory::ALL {
                let s = cat.saturation();
                prop_assert_eq!(s.apply(0.0), 0.0);
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                if lo < hi {
                    prop_assert!(s.apply(lo) < s.apply(hi));
                }
                prop_assert!(s.apply(hi) < s.limit());
            }
        }

        /// I1: with every producer complete, one more known finding strictly
        /// lowers the file score unless it already sits at the floor, and
        /// every finding's marginal gain is positive off the floor.
        #[test]
        fn i1_file_score_strictly_decreases_with_known_debt(
            base in prop::collection::vec(arb_finding(), 0..40),
            extra in arb_finding(),
        ) {
            let before: Vec<_> = base.iter().enumerate().map(|(i, &f)| finding(i as i64, f)).collect();
            let mut after = before.clone();
            after.push(finding(before.len() as i64, extra));
            let (b, a) = (upper(&before), upper(&after));
            prop_assert!(
                a.value.upper() < b.value.upper() || b.value.upper() == MIN_SCORE,
                "before {} after {}", b.value.upper(), a.value.upper()
            );
            for d in &a.deductions {
                prop_assert!(d.marginal > 0.0 || a.value.upper() == MIN_SCORE);
            }
            // Shares may exceed raw at low debt (the curve's initial slope is
            // 1/CATEGORY_SCALE_FRACTION) but always add up to the category.
            for c in &a.categories {
                prop_assert!(c.known < c.category.cap());
                let shares: f64 = a
                    .deductions
                    .iter()
                    .filter(|d| d.category == c.category)
                    .map(|d| d.scaled_deduction)
                    .sum();
                prop_assert!((shares - c.known).abs() < 1e-9);
            }
        }

        /// I2: a component never improves when a member worsens or the
        /// instability penalty rises (NLOC and membership fixed); the count
        /// term makes a worsening member strictly worse off the floor.
        #[test]
        fn i2_component_is_monotone_in_member_debt_and_penalty(
            members in arb_members(),
            pick in any::<prop::sample::Index>(),
            worsen in 0.001_f64..9.0,
            penalty in 0.0_f64..0.5,
            more_penalty in 0.0_f64..0.5,
        ) {
            let before = component_score(&members, penalty);
            prop_assert!(component_score(&members, penalty + more_penalty) <= before);
            if !members.is_empty() {
                let i = pick.index(members.len());
                let mut worse = members.clone();
                worse[i].0 = (worse[i].0 - worsen).max(MIN_SCORE);
                if worse[i].0 < members[i].0 {
                    let after = component_score(&worse, penalty);
                    prop_assert!(after < before || before == MIN_SCORE, "before {before} after {after}");
                }
            }
        }

        /// I3: no amount of clean NLOC dilutes a component below its count
        /// term's share of the debt.
        #[test]
        fn i3_clean_additions_cannot_dilute_the_count_term(
            members in arb_members(),
            clean_nloc in prop::collection::vec(1_i64..1_000_000_000, 0..8),
        ) {
            let mass: f64 = members.iter().map(|(s, _)| MAX_SCORE - s).sum();
            let floor = (1.0 - COMPONENT_DENSITY_SHARE) * COMPONENT_COUNT.apply(mass);
            let mut diluted = members.clone();
            diluted.extend(clean_nloc.iter().map(|&n| (MAX_SCORE, n)));
            let score = component_score(&diluted, 0.0);
            prop_assert!(score <= (MAX_SCORE - floor).max(MIN_SCORE) + 1e-9);
        }

        /// I4: component bounds computed from member lower/upper bounds (and
        /// the extreme penalties) contain the score of every member
        /// realization inside those bounds.
        #[test]
        fn i4_member_bounds_yield_sound_component_bounds(
            members in prop::collection::vec(
                (MIN_SCORE..=MAX_SCORE, MIN_SCORE..=MAX_SCORE, 0.0_f64..=1.0, 0_i64..5_000),
                0..12,
            ),
            penalty_at in 0.0_f64..=1.0,
        ) {
            let lower: Vec<_> = members.iter().map(|&(a, b, _, n)| (a.min(b), n)).collect();
            let upper: Vec<_> = members.iter().map(|&(a, b, _, n)| (a.max(b), n)).collect();
            let actual: Vec<_> = members
                .iter()
                .map(|&(a, b, t, n)| (a.min(b) + t * (a - b).abs(), n))
                .collect();
            let max_penalty = instability_penalty(1.0);
            let score = component_score(&actual, penalty_at * max_penalty);
            prop_assert!(component_score(&lower, max_penalty) <= score + 1e-9);
            prop_assert!(score <= component_score(&upper, 0.0) + 1e-9);
        }
    }
}
