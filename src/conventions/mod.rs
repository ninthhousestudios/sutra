mod attributes;
mod bitset;
mod context;
mod engine;

pub use attributes::{enrich_with_effects, extract_attrs_for_symbol, extract_cross_language_attrs, EffectPattern, ResolvedCallee};
pub use context::{FormalContext, Implication};
pub use engine::{Convention, ConventionViolation, FcaEngine, SymbolAttrs, MIN_CONFIDENCE, deduplicate_component_conventions};
