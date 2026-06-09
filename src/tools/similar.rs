use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::db::ResolveResult;
use crate::diagnostics::{CandidateInfo, Diagnostic};
use crate::error::{Result, SutraError};
use crate::similarity::search;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimilarArgs {
    pub workspace: String,
    /// Symbol name to find similar functions for
    pub symbol: String,
    /// Similarity mode: "strip" (structural shape only, default) or "embed" (structure + identifiers)
    #[serde(default)]
    pub mode: Option<String>,
    /// Maximum number of results (default: 10)
    #[serde(default)]
    pub limit: Option<usize>,
    /// Minimum similarity threshold 0.0-1.0 (default: 0.3)
    #[serde(default)]
    pub threshold: Option<f64>,
}

pub fn handle(
    db: &Db,
    symbol: &str,
    mode: Option<&str>,
    limit: Option<usize>,
    threshold: Option<f64>,
) -> Result<serde_json::Value> {
    let mode = mode.unwrap_or("strip");
    if mode != "strip" && mode != "embed" {
        return Err(SutraError::InvalidArgument {
            tool: "sutra_similar",
            argument: "mode",
            constraint: "must be \"strip\" or \"embed\"".to_string(),
            received: Some(mode.to_string()),
            next_action: "Retry with mode=\"strip\" or mode=\"embed\".".to_string(),
        });
    }
    let limit = limit.unwrap_or(10);
    let threshold = threshold.unwrap_or(0.3);

    let sym = match db.resolve_symbol_diagnostic(symbol, None)? {
        ResolveResult::Unique(s) => s,
        ResolveResult::NotFound => {
            return Ok(json!({
                "symbol": symbol,
                "diagnostic": serde_json::to_value(Diagnostic::NoSuchSymbol {
                    queried_name: symbol.to_string(),
                    queried_kind: None,
                    indexed_kinds: db.distinct_symbol_kinds().unwrap_or_default(),
                    freshness: None,
                    suggestion: "Use sutra_find to search by partial name, \
                                 or sutra_grep for a text search.".to_string(),
                }).unwrap(),
            }));
        }
        ResolveResult::Ambiguous(candidates) => {
            let infos: Vec<CandidateInfo> = candidates
                .iter()
                .map(|c| CandidateInfo {
                    qualified_name: c.qualified_name.clone(),
                    kind: c.kind.clone(),
                    file: db
                        .file_by_id(c.file_id)
                        .ok()
                        .flatten()
                        .map(|f| f.path)
                        .unwrap_or_default(),
                })
                .collect();
            return Ok(json!({
                "symbol": symbol,
                "diagnostic": serde_json::to_value(Diagnostic::Ambiguous {
                    queried_name: symbol.to_string(),
                    candidates: infos,
                    freshness: None,
                    suggestion: "Use the fully qualified name to disambiguate.".to_string(),
                }).unwrap(),
            }));
        }
    };

    if sym.kind != "function" && sym.kind != "method" {
        return Ok(json!({
            "symbol": sym.qualified_name,
            "kind": sym.kind,
            "diagnostic": "Similarity search only works on functions and methods. \
                           This symbol is a ".to_string() + &sym.kind + ".",
        }));
    }

    let query_vec = match db.load_hrr_vector(sym.id, mode)? {
        Some(v) => v,
        None => {
            return Ok(json!({
                "symbol": sym.qualified_name,
                "mode": mode,
                "diagnostic": "No HRR vector found for this symbol. \
                               Try reparsing the workspace with sutra_add_root.",
            }));
        }
    };

    let candidates = db.load_all_vectors_by_mode(mode)?;
    let results = search::find_similar(sym.id, &query_vec, &candidates, limit, threshold);

    if results.is_empty() {
        return Ok(json!({
            "query_symbol": sym.qualified_name,
            "mode": mode,
            "matches": [],
            "total": 0,
            "threshold": threshold,
            "limit": limit,
        }));
    }

    let match_ids: Vec<i64> = results.iter().map(|m| m.symbol_id).collect();
    let summaries = db.symbols_by_ids(&match_ids)?;

    let mut matches: Vec<serde_json::Value> = Vec::with_capacity(results.len());
    for m in &results {
        let summary = summaries.iter().find(|s| s.id == m.symbol_id);
        let score = (m.score * 1000.0).round() / 1000.0;
        matches.push(if let Some(s) = summary {
            json!({
                "symbol": s.qualified_name,
                "file": s.file_path,
                "lines": format!("{}-{}", s.start_line, s.end_line),
                "similarity": score,
            })
        } else {
            json!({
                "symbol_id": m.symbol_id,
                "similarity": score,
            })
        });
    }

    Ok(json!({
        "query_symbol": sym.qualified_name,
        "mode": mode,
        "matches": matches,
        "total": matches.len(),
        "threshold": threshold,
        "limit": limit,
    }))
}
