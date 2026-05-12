use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CandidateInfo {
    pub qualified_name: String,
    pub kind: String,
    pub file: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Diagnostic {
    NoSuchSymbol {
        queried_name: String,
        queried_kind: Option<String>,
        indexed_kinds: Vec<String>,
        suggestion: String,
    },
    Ambiguous {
        queried_name: String,
        candidates: Vec<CandidateInfo>,
        suggestion: String,
    },
    Stale {
        file: String,
        staleness_seconds: i64,
        suggestion: String,
    },
    AnalysisTierDisabled {
        tool: String,
        suggestion: String,
    },
    PartialResolution {
        resolved_name: String,
        unresolved_count: usize,
        suggestion: String,
    },
    SymbolExistsWithNoResults {
        symbol: String,
        symbol_kind: String,
        tool: String,
        suggestion: String,
    },
}

impl Diagnostic {
    pub fn suggest_next_query(&self) -> &str {
        match self {
            Diagnostic::NoSuchSymbol { suggestion, .. }
            | Diagnostic::Ambiguous { suggestion, .. }
            | Diagnostic::Stale { suggestion, .. }
            | Diagnostic::AnalysisTierDisabled { suggestion, .. }
            | Diagnostic::PartialResolution { suggestion, .. }
            | Diagnostic::SymbolExistsWithNoResults { suggestion, .. } => suggestion,
        }
    }
}
