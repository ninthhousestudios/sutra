use std::collections::HashMap;

use crate::db::{Db, HealthFindingRow};
use crate::error::Result;
use crate::health::findings::{BiomarkerKind, HealthSeverity};
use crate::health::instability::{self, ComponentInstability};

const BASE_SCORE: f64 = 10.0;
const MIN_SCORE: f64 = 1.0;
const MAX_SCORE: f64 = 10.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HealthCategory {
    Organizational,
    Structural,
    Coupling,
    Freshness,
    Coverage,
}

impl HealthCategory {
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

/// Whether git history is available to the git-organizational and churn
/// biomarkers, and if not, *why*. The distinction is load-bearing (sutra/408):
/// a non-git project can never run these producers (exclude them), but a git
/// project whose history we merely failed to read this run must be worst-cased,
/// not excluded — otherwise a transient `git log` failure removes debt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitAvailability {
    /// Git history ingested — the git biomarkers ran against real data.
    Available,
    /// A git repo, but no usable history this run (empty commit window or a
    /// git-command failure). The producers should have run but had no data →
    /// their dimensions are worst-cased, not excluded.
    NoHistory,
    /// Not a git repository at all — a true structural absence. The git
    /// biomarkers can never run here and are excluded (Unsupported).
    NotARepo,
}

impl GitAvailability {
    fn from_persisted(s: &str) -> Option<Self> {
        match s {
            "available" => Some(Self::Available),
            "no_history" => Some(Self::NoHistory),
            "not_a_repo" => Some(Self::NotARepo),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::NoHistory => "no_history",
            Self::NotARepo => "not_a_repo",
        }
    }
}

/// Workspace-scoped facts that decide whether a biomarker's data source exists
/// at all. Detected once per scoring pass.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceFacts {
    /// Git availability for the git-organizational and churn biomarkers.
    pub git: GitAvailability,
}

impl WorkspaceFacts {
    pub fn detect(db: &Db) -> Result<Self> {
        // Prefer the state persisted at the last full parse (where the git
        // outcome was actually observed). Fall back to the pre-sutra/408
        // commit-count heuristic for indexes that predate the column: a
        // populated commit table reads as Available, an empty one as NotARepo
        // (the old "no git" → Unsupported behavior).
        let git = match db.git_availability()? {
            Some(s) => GitAvailability::from_persisted(&s).unwrap_or(GitAvailability::NotARepo),
            None => {
                if db.commit_file_count()? > 0 {
                    GitAvailability::Available
                } else {
                    GitAvailability::NotARepo
                }
            }
        };
        Ok(Self { git })
    }
}

/// Whether a biomarker contributes to a file's score, and if not, why. This is
/// the "missing analysis is never zero debt" contract: a biomarker that should
/// have run but didn't must lower the score, not silently pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiomarkerSupport {
    /// Producer wired and its data source present in this workspace.
    Scored,
    /// Data source structurally absent at workspace scope (no git history, no
    /// coverage ingestion). Excluded from scoring and surfaced as a note —
    /// worst-casing every file on a dimension that can never run is noise.
    Unsupported(&'static str),
    /// The biomarker should be scored for this workspace but has no data to
    /// score from this run — either no producer emits it, or its data source
    /// exists but was unavailable (git history that failed to load or an empty
    /// commit window). Worst-cased at full weight and flags the file `partial`.
    Unwired,
}

impl BiomarkerKind {
    /// Classify a biomarker for FILE-LEVEL parse-time scoring. `None` means it
    /// is scored elsewhere (review-time on-demand, or component-scoped) and is
    /// not part of the per-file worst-case contract. Single exhaustive source of
    /// truth: adding a variant forces a decision here, so a new biomarker can
    /// never silently score as zero debt.
    pub fn file_scoring_support(&self, facts: &WorkspaceFacts) -> Option<BiomarkerSupport> {
        let git = || match facts.git {
            GitAvailability::Available => BiomarkerSupport::Scored,
            // Repo exists but no history this run → worst-case, don't exclude.
            GitAvailability::NoHistory => BiomarkerSupport::Unwired,
            // Not a git repo → the producer can never run here → exclude.
            GitAvailability::NotARepo => BiomarkerSupport::Unsupported("not a git repository"),
        };
        match self {
            Self::NestedComplexity | Self::ImportCycle | Self::DeadCodeRatio => {
                Some(BiomarkerSupport::Scored)
            }
            Self::CoChangeScatter
            | Self::ChangeEntropy
            | Self::OwnershipRisk
            | Self::HiddenCoupling
            | Self::BlastRadiusChurn => Some(git()),
            Self::CoverageGradient => {
                Some(BiomarkerSupport::Unsupported("no coverage data source"))
            }
            // Review-time on-demand (function_hotspot, code_age_volatility,
            // hrr_shape_change) and component-scoped (component_instability) are
            // scored in their own paths, not per-file at parse time.
            Self::FunctionHotspot
            | Self::CodeAgeVolatility
            | Self::HrrShapeChange
            | Self::ComponentInstability => None,
        }
    }
}

#[derive(Debug)]
pub struct FindingDeduction {
    pub finding_id: i64,
    pub raw_deduction: f64,
    pub scaled_deduction: f64,
    pub category: HealthCategory,
}

/// A worst-case deduction for a file-scored biomarker that had no producer
/// (Unwired). Not tied to a finding — it exists precisely because no finding
/// was emitted — but it lowers the score and flags the file `partial`.
#[derive(Debug, Clone)]
pub struct MissingDeduction {
    pub biomarker: BiomarkerKind,
    pub category: HealthCategory,
    pub scaled_deduction: f64,
}

#[derive(Debug)]
pub struct FileHealthScore {
    pub score: f64,
    pub deductions: Vec<FindingDeduction>,
    /// Worst-cased biomarkers whose producer never ran. Non-empty ⇒ the score is
    /// `partial`: an apparently clean number computed from incomplete analysis.
    pub missing: Vec<MissingDeduction>,
}

impl FileHealthScore {
    pub fn partial(&self) -> bool {
        !self.missing.is_empty()
    }
}

/// Score one file. `covered` is whether this file's findings are valid for its
/// *current* content (sutra/408): its health_coverage stamp matches its
/// content_hash. When false — an incrementally reparsed file whose findings
/// were never recomputed, or a newly added file with none — the present
/// findings describe stale content and are ignored; every file-scored biomarker
/// that could run is worst-cased instead, so a not-yet-analyzed file can never
/// float up to a clean 10.0.
pub fn score_file(
    findings: &[HealthFindingRow],
    facts: &WorkspaceFacts,
    covered: bool,
) -> FileHealthScore {
    // Present findings: real debt the producers actually measured. Only trusted
    // when the analysis is current for this file's content; otherwise the file
    // is worst-cased wholesale below.
    let mut present: HashMap<HealthCategory, Vec<(usize, f64)>> = HashMap::new();
    if covered {
        for (i, f) in findings.iter().enumerate() {
            let Some(kind) = BiomarkerKind::parse(&f.biomarker_kind) else {
                continue;
            };
            let Some(severity) = HealthSeverity::parse(&f.severity) else {
                continue;
            };
            let raw = severity.weight() * kind.default_weight();
            present.entry(kind.category()).or_default().push((i, raw));
        }
    }

    // Missing analysis is never zero debt. A file-scored biomarker is worst-
    // cased at full weight when it is Unwired (no producer, or its data source
    // was unavailable this run), or when it is Scored but this file's analysis
    // is not current (`!covered`). Unsupported ones (data source structurally
    // absent — not a git repo, no coverage ingestion) are excluded, not worst-
    // cased: worst-casing a dimension that can never run is noise.
    let mut missing_raw: HashMap<HealthCategory, Vec<(BiomarkerKind, f64)>> = HashMap::new();
    for kind in BiomarkerKind::ALL {
        let worst_case = match kind.file_scoring_support(facts) {
            Some(BiomarkerSupport::Unwired) => true,
            Some(BiomarkerSupport::Scored) => !covered,
            Some(BiomarkerSupport::Unsupported(_)) | None => false,
        };
        if worst_case {
            let raw = kind.default_severity().weight() * kind.default_weight();
            missing_raw
                .entry(kind.category())
                .or_default()
                .push((kind, raw));
        }
    }

    let mut deductions = Vec::new();
    let mut missing = Vec::new();
    let mut total_deduction = 0.0;

    let mut categories: Vec<HealthCategory> = present.keys().copied().collect();
    for cat in missing_raw.keys() {
        if !categories.contains(cat) {
            categories.push(*cat);
        }
    }

    for cat in categories {
        let present_items = present.get(&cat).map(Vec::as_slice).unwrap_or(&[]);
        let missing_items = missing_raw.get(&cat).map(Vec::as_slice).unwrap_or(&[]);
        let raw_total: f64 = present_items.iter().map(|(_, r)| r).sum::<f64>()
            + missing_items.iter().map(|(_, r)| r).sum::<f64>();
        let scale = if raw_total > cat.cap() {
            cat.cap() / raw_total
        } else {
            1.0
        };

        for &(idx, raw) in present_items {
            let scaled = raw * scale;
            deductions.push(FindingDeduction {
                finding_id: findings[idx].id,
                raw_deduction: raw,
                scaled_deduction: scaled,
                category: cat,
            });
            total_deduction += scaled;
        }
        for &(kind, raw) in missing_items {
            let scaled = raw * scale;
            missing.push(MissingDeduction {
                biomarker: kind,
                category: cat,
                scaled_deduction: scaled,
            });
            total_deduction += scaled;
        }
    }

    FileHealthScore {
        score: (BASE_SCORE - total_deduction).clamp(MIN_SCORE, MAX_SCORE),
        deductions,
        missing,
    }
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

#[derive(Debug)]
pub struct ScoredFile {
    pub file_id: i64,
    pub score: f64,
    pub deductions: Vec<FindingDeduction>,
    /// Per-category scaled deduction totals, including worst-cased `missing`
    /// biomarkers so the breakdown always sums to `BASE_SCORE - score`.
    pub category_totals: HashMap<HealthCategory, f64>,
    /// Worst-cased biomarkers whose producer never ran (non-empty ⇒ partial).
    pub missing: Vec<MissingDeduction>,
}

#[derive(Debug)]
pub struct ScoredComponent {
    pub component_id: String,
    pub component_name: String,
    pub score: f64,
    pub member_count: usize,
    pub total_nloc: i64,
    pub instability: Option<ComponentInstability>,
}

#[derive(Debug)]
pub struct WorkspaceHealth {
    pub file_scores: Vec<ScoredFile>,
    pub component_scores: Vec<ScoredComponent>,
    pub comp_file_ids: HashMap<String, Vec<i64>>,
}

pub fn score_workspace(db: &Db) -> Result<WorkspaceHealth> {
    let all_with_waivers = db.get_health_findings_with_waiver_status()?;

    let mut findings_by_file: HashMap<i64, Vec<HealthFindingRow>> = HashMap::new();
    for (finding, waived) in all_with_waivers {
        if !waived {
            findings_by_file
                .entry(finding.file_id)
                .or_default()
                .push(finding);
        }
    }

    let facts = WorkspaceFacts::detect(db)?;
    // Score EVERY indexed file, not just files that happen to have findings
    // (sutra/408). A finding-free file that was genuinely analyzed scores a
    // clean 10.0; a file whose analysis is stale or absent is worst-cased via
    // `covered=false` — the old loop skipped it entirely and it floored at
    // BASE_SCORE, silently reading incomplete analysis as zero debt.
    let coverage = db.health_coverage_map()?;
    let all_files = db.all_files()?;
    let empty_findings: Vec<HealthFindingRow> = Vec::new();
    let mut file_scores = Vec::new();
    for file in &all_files {
        let findings = findings_by_file.get(&file.id).unwrap_or(&empty_findings);
        let covered = coverage
            .get(&file.id)
            .is_some_and(|stamp| *stamp == file.content_hash);
        let result = score_file(findings, &facts, covered);
        let mut category_totals: HashMap<HealthCategory, f64> = HashMap::new();
        for d in &result.deductions {
            *category_totals.entry(d.category).or_default() += d.scaled_deduction;
        }
        for m in &result.missing {
            *category_totals.entry(m.category).or_default() += m.scaled_deduction;
        }
        file_scores.push(ScoredFile {
            file_id: file.id,
            score: result.score,
            deductions: result.deductions,
            category_totals,
            missing: result.missing,
        });
    }

    let components = db.all_components()?;
    let memberships = db.component_members_with_line_count()?;
    // Instability is always computed: it feeds the component score (not just
    // decorative metadata), so the snapshot and file_health paths must agree.
    let instability_map = instability::compute_component_instability(db).unwrap_or_default();

    let file_score_map: HashMap<i64, f64> = file_scores
        .iter()
        .map(|fs| (fs.file_id, fs.score))
        .collect();

    let mut comp_files: HashMap<&str, Vec<(i64, i64)>> = HashMap::new();
    let mut comp_file_ids: HashMap<String, Vec<i64>> = HashMap::new();
    for (comp_id, file_id, line_count) in &memberships {
        comp_files
            .entry(comp_id.as_str())
            .or_default()
            .push((*file_id, *line_count));
        comp_file_ids
            .entry(comp_id.clone())
            .or_default()
            .push(*file_id);
    }

    let mut component_scores = Vec::new();
    for comp in &components {
        let Some(members) = comp_files.get(comp.id.as_str()) else {
            continue;
        };
        let pairs: Vec<(f64, i64)> = members
            .iter()
            .map(|&(fid, lc)| (*file_score_map.get(&fid).unwrap_or(&BASE_SCORE), lc))
            .collect();
        let total_nloc: i64 = pairs.iter().map(|(_, n)| n).sum();
        let base = score_component(&pairs);
        let instability = instability_map.get(&comp.id).cloned();
        let comp_score = match &instability {
            Some(inst) => {
                (base - instability_penalty(inst.instability)).clamp(MIN_SCORE, MAX_SCORE)
            }
            None => base,
        };
        component_scores.push(ScoredComponent {
            component_id: comp.id.clone(),
            component_name: comp.name.clone(),
            score: comp_score,
            member_count: members.len(),
            total_nloc,
            instability,
        });
    }

    Ok(WorkspaceHealth {
        file_scores,
        component_scores,
        comp_file_ids,
    })
}
