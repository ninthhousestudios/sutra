pub mod erosion;
pub mod findings;
pub mod git_metrics;

pub use findings::*;
pub use git_metrics::{
    OwnersConfig, compute_blast_radius_churn, compute_change_entropy, compute_co_change_scatter,
    compute_hidden_coupling, compute_ownership_risk,
};
