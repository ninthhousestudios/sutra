mod attributes;
mod bitset;
mod context;
mod engine;

pub use attributes::extract_symbol_attrs;
pub use context::{FormalContext, Implication};
pub use engine::{Convention, ConventionViolation, FcaEngine, SymbolAttrs};
