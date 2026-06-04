use std::collections::HashMap;

use crate::db::HealthFindingRow;
use crate::health::findings::{BiomarkerKind, HealthSeverity};

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
            Self::HiddenCoupling | Self::ComponentInstability => HealthCategory::Coupling,
            Self::CodeAgeVolatility | Self::HrrShapeChange => HealthCategory::Freshness,
            Self::DeadCodeRatio | Self::CoverageGradient | Self::ConventionDrift => {
                HealthCategory::Coverage
            }
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
            Self::ConventionDrift => 0.50,
            Self::ComponentInstability => 0.50,
            Self::HrrShapeChange => 0.50,
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

#[derive(Debug)]
pub struct FindingDeduction {
    pub finding_id: i64,
    pub raw_deduction: f64,
    pub scaled_deduction: f64,
    pub category: HealthCategory,
}

#[derive(Debug)]
pub struct FileHealthScore {
    pub score: f64,
    pub deductions: Vec<FindingDeduction>,
}

pub fn score_file(findings: &[HealthFindingRow]) -> FileHealthScore {
    let mut by_category: HashMap<HealthCategory, Vec<(usize, f64)>> = HashMap::new();

    for (i, f) in findings.iter().enumerate() {
        let Some(kind) = BiomarkerKind::from_str(&f.biomarker_kind) else {
            continue;
        };
        let Some(severity) = HealthSeverity::from_str(&f.severity) else {
            continue;
        };
        let raw = severity.weight() * kind.default_weight();
        by_category
            .entry(kind.category())
            .or_default()
            .push((i, raw));
    }

    let mut deductions = Vec::new();
    let mut total_deduction = 0.0;

    for (cat, items) in &by_category {
        let raw_total: f64 = items.iter().map(|(_, r)| r).sum();
        let scale = if raw_total > cat.cap() {
            cat.cap() / raw_total
        } else {
            1.0
        };

        for &(idx, raw) in items {
            let scaled = raw * scale;
            deductions.push(FindingDeduction {
                finding_id: findings[idx].id,
                raw_deduction: raw,
                scaled_deduction: scaled,
                category: *cat,
            });
            total_deduction += scaled;
        }
    }

    FileHealthScore {
        score: (BASE_SCORE - total_deduction).clamp(MIN_SCORE, MAX_SCORE),
        deductions,
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
