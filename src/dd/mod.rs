mod engine;
mod worker;

pub use engine::DdEngine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    pub file_ids: Vec<i64>,
}

#[derive(Debug, Clone, Default)]
pub struct DdFacts {
    pub import_edges: Vec<(i64, i64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstraintViolation {
    pub from_id: i64,
    pub to_id: i64,
    pub rule_from: String,
    pub rule_to: String,
}

#[derive(Debug, Clone, Default)]
pub struct DdDelta {
    pub added_edges: Vec<(i64, i64)>,
    pub removed_edges: Vec<(i64, i64)>,
}
