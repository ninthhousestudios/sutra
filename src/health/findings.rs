use crate::db::Db;
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthSeverity {
    Advisory,
    Informational,
}

impl HealthSeverity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Advisory => "advisory",
            Self::Informational => "informational",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiomarkerKind {
    NestedComplexity,
    CoChangeScatter,
    ChangeEntropy,
    OwnershipRisk,
    FunctionHotspot,
    HiddenCoupling,
    BlastRadiusChurn,
    DeadCodeRatio,
    CodeAgeVolatility,
    CoverageGradient,
    ConventionDrift,
    ComponentInstability,
    HrrShapeChange,
}

impl BiomarkerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NestedComplexity => "nested_complexity",
            Self::CoChangeScatter => "co_change_scatter",
            Self::ChangeEntropy => "change_entropy",
            Self::OwnershipRisk => "ownership_risk",
            Self::FunctionHotspot => "function_hotspot",
            Self::HiddenCoupling => "hidden_coupling",
            Self::BlastRadiusChurn => "blast_radius_churn",
            Self::DeadCodeRatio => "dead_code_ratio",
            Self::CodeAgeVolatility => "code_age_volatility",
            Self::CoverageGradient => "coverage_gradient",
            Self::ConventionDrift => "convention_drift",
            Self::ComponentInstability => "component_instability",
            Self::HrrShapeChange => "hrr_shape_change",
        }
    }

    pub fn default_severity(&self) -> HealthSeverity {
        match self {
            Self::DeadCodeRatio
            | Self::CodeAgeVolatility
            | Self::CoverageGradient
            | Self::ConventionDrift
            | Self::ComponentInstability
            | Self::HrrShapeChange => HealthSeverity::Informational,
            _ => HealthSeverity::Advisory,
        }
    }
}

#[derive(Debug, Clone)]
pub struct HealthFinding {
    pub file_id: i64,
    pub symbol_id: Option<i64>,
    pub biomarker_kind: BiomarkerKind,
    pub severity: HealthSeverity,
    pub confidence: f64,
    pub provenance: String,
    pub metric_value: f64,
    pub threshold: f64,
    pub detail: String,
}

const NESTING_THRESHOLD: i64 = 4;

pub fn compute_nested_complexity(db: &Db) -> Result<Vec<HealthFinding>> {
    let rows = db.symbols_exceeding_nesting(NESTING_THRESHOLD)?;
    let findings = rows
        .into_iter()
        .map(|row| HealthFinding {
            file_id: row.file_id,
            symbol_id: Some(row.symbol_id),
            biomarker_kind: BiomarkerKind::NestedComplexity,
            severity: HealthSeverity::Advisory,
            confidence: 1.0,
            provenance: "computed".to_string(),
            metric_value: row.max_nesting as f64,
            threshold: NESTING_THRESHOLD as f64,
            detail: format!(
                "{} has nesting depth {} (threshold {})",
                row.qualified_name, row.max_nesting, NESTING_THRESHOLD
            ),
        })
        .collect();
    Ok(findings)
}

pub fn compute_all_health_findings(db: &Db) -> Result<Vec<HealthFinding>> {
    compute_nested_complexity(db)
}
