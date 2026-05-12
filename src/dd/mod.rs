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

#[derive(Debug, Clone, Default)]
pub struct DdDelta {
    pub added_edges: Vec<(i64, i64)>,
    pub removed_edges: Vec<(i64, i64)>,
}
