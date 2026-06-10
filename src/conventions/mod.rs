mod attributes;
mod bitset;
mod context;
pub mod drift;
mod engine;
pub mod lifecycle;
pub mod pipeline;
pub mod templates;

pub use attributes::{
    EffectPattern, ResolvedCallee, enrich_with_effects, extract_attrs_for_symbol,
    extract_cross_language_attrs,
};
pub use context::{FormalContext, Implication};
pub use drift::{
    DRIFT_THRESHOLD, DRIFT_WINDOW, DivergingAttribute, DriftAlert, find_diverging_attributes,
};
pub use engine::{
    Convention, ConventionMatch, ConventionViolation, FcaEngine, MIN_CONFIDENCE, SymbolAttrs,
    component_min_support, deduplicate_component_conventions,
};
