pub mod assess;
pub mod compare;
pub mod erosion;
pub mod evidence;
pub mod findings;
pub mod git_metrics;
pub mod instability;
pub mod ondemand;
pub mod probe;
pub mod refresh;
pub mod scoring;

pub use findings::*;
pub use git_metrics::{
    OwnersConfig, compute_blast_radius_churn, compute_change_entropy, compute_co_change_scatter,
    compute_hidden_coupling, compute_ownership_risk,
};
pub use scoring::{FileHealthScore, FindingDeduction, HealthCategory, score_component, score_file};
