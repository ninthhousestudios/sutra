pub mod findings;
pub mod git_metrics;
pub mod ondemand;
pub mod scoring;

pub use findings::*;
pub use git_metrics::{
    compute_change_entropy, compute_co_change_scatter, compute_hidden_coupling,
    compute_ownership_risk, load_owners_config, OwnersConfig,
};
pub use scoring::{score_component, score_file, FileHealthScore, FindingDeduction, HealthCategory};
