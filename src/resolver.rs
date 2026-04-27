use crate::parser::{ExtractedRef, ExtractedSymbol};

#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub original: ExtractedRef,
    pub target_symbol_id: Option<i64>,
    pub unresolved_name: Option<String>,
}

pub fn resolve_refs(
    _symbols: &[ExtractedSymbol],
    _refs: &[ExtractedRef],
    _existing_symbols: &[(i64, String, String)],
) -> Vec<ResolvedRef> {
    todo!("Issue 5")
}
