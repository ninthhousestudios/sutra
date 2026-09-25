use std::collections::HashMap;
use std::path::Path;

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

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "advisory" => Some(Self::Advisory),
            "informational" => Some(Self::Informational),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BiomarkerKind {
    NestedComplexity,
    CoChangeScatter,
    ChangeEntropy,
    OwnershipRisk,
    HiddenCoupling,
    BlastRadiusChurn,
    DeadCodeRatio,
    ImportCycle,
}

impl BiomarkerKind {
    /// Every variant, for exhaustive iteration. Kept adjacent to the enum so a
    /// new variant is added here too.
    pub const ALL: [BiomarkerKind; 8] = [
        Self::NestedComplexity,
        Self::CoChangeScatter,
        Self::ChangeEntropy,
        Self::OwnershipRisk,
        Self::HiddenCoupling,
        Self::BlastRadiusChurn,
        Self::DeadCodeRatio,
        Self::ImportCycle,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NestedComplexity => "nested_complexity",
            Self::CoChangeScatter => "co_change_scatter",
            Self::ChangeEntropy => "change_entropy",
            Self::OwnershipRisk => "ownership_risk",
            Self::HiddenCoupling => "hidden_coupling",
            Self::BlastRadiusChurn => "blast_radius_churn",
            Self::DeadCodeRatio => "dead_code_ratio",
            Self::ImportCycle => "import_cycle",
        }
    }

    pub fn default_severity(&self) -> HealthSeverity {
        match self {
            Self::DeadCodeRatio | Self::ImportCycle => HealthSeverity::Informational,
            _ => HealthSeverity::Advisory,
        }
    }

    /// Whether the producer consumes git history. When history is not loaded
    /// these have no current data and emit no findings.
    pub fn needs_history(self) -> bool {
        matches!(
            self,
            Self::CoChangeScatter
                | Self::ChangeEntropy
                | Self::OwnershipRisk
                | Self::HiddenCoupling
                | Self::BlastRadiusChurn
        )
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "nested_complexity" => Some(Self::NestedComplexity),
            "co_change_scatter" => Some(Self::CoChangeScatter),
            "change_entropy" => Some(Self::ChangeEntropy),
            "ownership_risk" => Some(Self::OwnershipRisk),
            "hidden_coupling" => Some(Self::HiddenCoupling),
            "blast_radius_churn" => Some(Self::BlastRadiusChurn),
            "dead_code_ratio" => Some(Self::DeadCodeRatio),
            "import_cycle" => Some(Self::ImportCycle),
            _ => None,
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

/// Recompute every persistent biomarker and replace the live `health_findings`
/// table. When history is not loaded the git producers have no current data, so
/// their findings are dropped rather than kept from retained commit rows.
pub fn refresh_findings(db: &Db, workspace_root: &Path, history_loaded: bool) -> Result<()> {
    let mut findings = compute_all_health_findings(db, workspace_root)?;
    if !history_loaded {
        findings.retain(|f| !f.biomarker_kind.needs_history());
    }
    db.replace_health_findings(&findings)
}

pub fn compute_all_health_findings(db: &Db, workspace_root: &Path) -> Result<Vec<HealthFinding>> {
    let mut findings = compute_nested_complexity(db)?;
    findings.extend(super::git_metrics::compute_co_change_scatter(db)?);
    findings.extend(super::git_metrics::compute_change_entropy(db)?);
    // A malformed/unreadable owners file means the ownership producer emits
    // nothing, never a score against a silent empty default.
    if let Some(owners) = super::git_metrics::load_owners(workspace_root) {
        findings.extend(super::git_metrics::compute_ownership_risk(db, &owners)?);
    }
    findings.extend(super::git_metrics::compute_hidden_coupling(db)?);
    findings.extend(compute_import_cycle_membership(db)?);
    findings.extend(compute_dead_code_ratio(db)?);
    findings.extend(super::git_metrics::compute_blast_radius_churn(db)?);
    Ok(findings)
}

/// Fraction of a file's local (non-pub, non-test) symbols that nothing
/// references. PROVISIONAL threshold (0.15): not repowise-calibrated — the
/// corpus is not available in this repo. Weight 0.80 (moderate), Informational.
const DEAD_CODE_RATIO_THRESHOLD: f64 = 0.15;

pub fn compute_dead_code_ratio(db: &Db) -> Result<Vec<HealthFinding>> {
    let rows = db.dead_code_ratio_by_file()?;
    let findings = rows
        .into_iter()
        .filter_map(|(file_id, dead, total)| {
            if total == 0 {
                return None;
            }
            let ratio = dead as f64 / total as f64;
            if ratio < DEAD_CODE_RATIO_THRESHOLD {
                return None;
            }
            Some(HealthFinding {
                file_id,
                symbol_id: None,
                biomarker_kind: BiomarkerKind::DeadCodeRatio,
                severity: BiomarkerKind::DeadCodeRatio.default_severity(),
                confidence: 1.0,
                provenance: "computed".into(),
                metric_value: ratio,
                threshold: DEAD_CODE_RATIO_THRESHOLD,
                detail: format!(
                    "{dead} of {total} local symbols are unreferenced ({:.0}%)",
                    ratio * 100.0
                ),
            })
        })
        .collect();
    Ok(findings)
}

fn compute_import_cycle_membership(db: &Db) -> Result<Vec<HealthFinding>> {
    let edges = db.import_edges()?;
    let sccs = crate::graph::find_import_sccs(&edges);

    let mut scc_count: HashMap<i64, usize> = HashMap::new();
    for scc in &sccs {
        for &fid in scc {
            *scc_count.entry(fid).or_default() += 1;
        }
    }

    let findings = scc_count
        .into_iter()
        .map(|(file_id, count)| HealthFinding {
            file_id,
            symbol_id: None,
            biomarker_kind: BiomarkerKind::ImportCycle,
            severity: BiomarkerKind::ImportCycle.default_severity(),
            confidence: 1.0,
            provenance: "computed".into(),
            metric_value: count as f64,
            threshold: 1.0,
            detail: format!("file participates in {count} import cycle group(s)"),
        })
        .collect();

    Ok(findings)
}
