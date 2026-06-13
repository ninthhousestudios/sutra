use crate::rules::Severity;

#[derive(Debug, Clone)]
pub struct ConstraintFinding {
    pub constraint_id: String,
    pub constraint_name: Option<String>,
    pub constraint_kind: String,
    pub severity: Severity,
    pub provenance: Option<String>,
    pub from_path: String,
    pub to_path: String,
    pub component_context: Option<String>,
    pub detail: String,
    pub delta: FindingDelta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingDelta {
    Unknown,
    PreExisting,
    Introduced,
    Resolved,
}
