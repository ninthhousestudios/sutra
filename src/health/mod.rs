pub mod findings;
pub mod scoring;

pub use findings::*;
pub use scoring::{score_component, score_file, FileHealthScore, FindingDeduction, HealthCategory};
