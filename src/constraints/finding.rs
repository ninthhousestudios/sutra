use std::sync::Arc;

use crate::rules::Severity;

#[derive(Debug, Clone)]
pub struct ConstraintFinding {
    pub constraint_id: Arc<str>,
    pub constraint_name: Option<Arc<str>>,
    pub constraint_kind: String,
    pub severity: Severity,
    pub provenance: Option<Arc<str>>,
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
