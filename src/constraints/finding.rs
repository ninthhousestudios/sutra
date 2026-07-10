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
    pub line: Option<u32>,
    pub snippet: Option<String>,
    pub enclosing_symbol: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingDelta {
    Unknown,
    PreExisting,
    Introduced,
    Resolved,
}
