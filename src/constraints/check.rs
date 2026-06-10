use std::collections::HashSet;

use crate::rules::{ConstraintParseError, Severity};

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

#[derive(Debug, Clone)]
pub struct WaivedFinding {
    pub finding: ConstraintFinding,
    pub rationale: String,
    pub waived_by: String,
}

#[derive(Debug, Clone, Default)]
pub struct CheckOutcome {
    pub active: Vec<ConstraintFinding>,
    pub waived: Vec<WaivedFinding>,
    pub resolved: Vec<ConstraintFinding>,
    pub parse_errors: Vec<ConstraintParseError>,
}

pub enum EvalScope<'a> {
    Workspace,
    ChangedFiles {
        changed_ids: &'a HashSet<i64>,
        old_edges: &'a HashSet<(i64, i64)>,
    },
    SingleFile(i64),
    Edges(&'a [(i64, i64)]),
}

pub enum FactsSource<'a> {
    DdBacked {
        db: &'a crate::db::Db,
        dd_engine: Option<&'a super::DdEngine>,
    },
    RawConn(&'a rusqlite::Connection),
}
