pub mod findings;
pub mod git_metrics;
pub mod instability;
pub mod ondemand;
pub mod scoring;

pub use findings::*;
pub use git_metrics::{
    OwnersConfig, compute_change_entropy, compute_co_change_scatter, compute_hidden_coupling,
    compute_ownership_risk, load_owners_config,
};
pub use scoring::{FileHealthScore, FindingDeduction, HealthCategory, score_component, score_file};
