use serde::Serialize;

use crate::freshness::FreshnessLevel;

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
        #[serde(skip_serializing_if = "Option::is_none")]
        freshness: Option<FreshnessLevel>,
        suggestion: String,
    },
    Ambiguous {
        queried_name: String,
        candidates: Vec<CandidateInfo>,
        #[serde(skip_serializing_if = "Option::is_none")]
        freshness: Option<FreshnessLevel>,
        suggestion: String,
    },
    Stale {
        file: String,
        staleness_seconds: i64,
        freshness: Option<FreshnessLevel>,
        suggestion: String,
    },
    AnalysisTierDisabled {
        tool: String,
        suggestion: String,
    },
    PartialResolution {
        resolved_name: String,
        unresolved_count: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        freshness: Option<FreshnessLevel>,
        suggestion: String,
    },
    SymbolExistsWithNoResults {
        symbol: String,
        symbol_kind: String,
        tool: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        freshness: Option<FreshnessLevel>,
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

    pub fn with_freshness(mut self, level: FreshnessLevel) -> Self {
        match &mut self {
            Diagnostic::NoSuchSymbol { freshness, .. }
            | Diagnostic::Ambiguous { freshness, .. }
            | Diagnostic::Stale { freshness, .. }
            | Diagnostic::PartialResolution { freshness, .. }
            | Diagnostic::SymbolExistsWithNoResults { freshness, .. } => {
                *freshness = Some(level);
            }
            Diagnostic::AnalysisTierDisabled { .. } => {}
        }
        self
    }
}
