mod attributes;
mod bitset;
mod context;
mod engine;
pub mod pipeline;

pub use attributes::{
    AttributeRole, EffectPattern, ResolvedCallee, classify_attribute, dart_effect_packages,
    enrich_all_effects, enrich_with_dart_import_effects, enrich_with_effects,
    extract_attrs_for_symbol, extract_cross_language_attrs,
};
pub use context::{FormalContext, Implication};
pub use engine::{
    Convention, ConventionMatch, ConventionViolation, Deviation, FcaEngine, MIN_CONFIDENCE,
    ObservedPattern, SymbolAttrs, component_min_support, deduplicate_component_conventions,
    describe_patterns, detect_deviations,
};
