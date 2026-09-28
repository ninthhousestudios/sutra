use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use std::path::Path;

use crate::db::Db;
use crate::db::ResolveResult;
use crate::diagnostics::{CandidateInfo, Diagnostic};
use crate::error::{Result, SutraError};
use crate::parser::adapter::LanguageRegistry;
use crate::similarity::search;
use crate::tools::dup_exists::{self, Neighbours};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SimilarArgs {
    #[serde(default)]
    pub workspace: String,
    /// Symbol name to find similar functions for. Omit to find all near-duplicate pattern families.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Similarity mode. "dup" (default): does this logic already exist? Ranks likely duplicates
    /// by identifiers, AST shape and rare shared code runs. "embed": HRR cosine on AST shape plus
    /// identifiers. "strip": same AST shape, identifiers ignored; nearly any two small functions
    /// of similar shape score high, so it is not a duplicate check.
    #[serde(default)]
    pub mode: Option<String>,
    /// Maximum number of results (default: 10 for symbol mode, all for duplicates mode)
    #[serde(default)]
    pub limit: Option<usize>,
    /// Minimum similarity threshold 0.0-1.0 (default: 0.3 for symbol mode, 0.85 for duplicates mode).
    /// In dup mode it filters only the matches that are not likely_duplicate: those are always
    /// returned, even below it, since a rare shared code run fires on its own.
    #[serde(default)]
    pub threshold: Option<f64>,
    /// Minimum group size for duplicate detection (default: 3). Only used when symbol is omitted.
    #[serde(default)]
    pub min_group: Option<usize>,
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    registry: &LanguageRegistry,
    args: &SimilarArgs,
) -> Result<serde_json::Value> {
    match args.symbol.as_deref() {
        Some(sym) => handle_similar(
            db,
            workspace_root,
            registry,
            sym,
            args.mode.as_deref(),
            args.limit,
            args.threshold,
        ),
        None => handle_duplicates(db, args.threshold, args.min_group),
    }
}

fn handle_similar(
    db: &Db,
    workspace_root: &Path,
    registry: &LanguageRegistry,
    symbol: &str,
    mode: Option<&str>,
    limit: Option<usize>,
    threshold: Option<f64>,
) -> Result<serde_json::Value> {
    let mode = mode.unwrap_or("dup");
    if !matches!(mode, "dup" | "embed" | "strip") {
        return Err(SutraError::InvalidArgument {
            tool: "sutra_similar",
            argument: "mode",
            constraint: "must be \"dup\", \"embed\" or \"strip\"".to_string(),
            received: Some(mode.to_string()),
            next_action: "Retry with mode=\"dup\" (does this already exist?), \"embed\" or \
                          \"strip\" (same AST shape)."
                .to_string(),
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
                    suggestion: "Use sutra_explore to search by partial name, \
                                 or sutra_lookup to match symbol names by pattern.".to_string(),
                }).unwrap(),
            }));
        }
        ResolveResult::Ambiguous(candidates) => {
            let infos: Vec<CandidateInfo> = candidates
                .iter()
                .map(|c| CandidateInfo {
                    qualified_name: c.qualified_name.to_string(),
                    kind: c.kind.to_string(),
                    file: db
                        .file_by_id(c.file_id)
                        .ok()
                        .flatten()
                        .map(|f| f.path.to_string())
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

    if &*sym.kind != "function" && &*sym.kind != "method" {
        return Ok(json!({
            "symbol": sym.qualified_name,
            "kind": sym.kind,
            "diagnostic": "Similarity search only works on functions and methods. \
                           This symbol is a ".to_string() + &sym.kind + ".",
        }));
    }

    if mode == "dup" {
        let found = dup_exists::neighbours(db, workspace_root, registry, sym.id, limit, threshold)?;
        return Ok(match found {
            Some(n) => neighbours_json(&n, threshold, limit),
            None => json!({
                "symbol": sym.qualified_name,
                "mode": mode,
                "diagnostic": "This function is not indexed yet. Try reparsing the workspace.",
            }),
        });
    }

    let query_vec = match db.load_hrr_vector(sym.id, mode)? {
        Some(v) => v,
        None => {
            return Ok(json!({
                "symbol": sym.qualified_name,
                "mode": mode,
                "diagnostic": "No HRR vector found for this symbol. \
                               Try reparsing the workspace.",
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

fn neighbours_json(n: &Neighbours, threshold: f64, limit: usize) -> serde_json::Value {
    let matches: Vec<serde_json::Value> = n
        .matches
        .iter()
        .map(|m| {
            let f = n.matched(m);
            json!({
                "symbol": f.qualified_name,
                "file": f.hrr.file_path,
                "lines": format!("{}-{}", f.hrr.start_line, f.hrr.end_line),
                "similarity": m.combo,
                "embed": m.embed,
                "lex": m.lex,
                "shared_runs": m.shared_runs,
                "likely_duplicate": n.likely_duplicate(m),
                "shared": m.shared,
            })
        })
        .collect();
    let mut out = json!({
        "query_symbol": n.query().qualified_name,
        "mode": "dup",
        "matches": matches,
        "total": matches.len(),
        "candidates": n.candidates,
        "threshold": threshold,
        "limit": limit,
    });
    if !n.incomplete.is_empty() {
        out["incomplete"] = json!(n.incomplete);
    }
    out
}

fn handle_duplicates(
    db: &Db,
    threshold: Option<f64>,
    min_group: Option<usize>,
) -> Result<serde_json::Value> {
    let threshold = threshold.unwrap_or(0.85);
    let min_group = min_group.unwrap_or(3);

    let mut families = Vec::new();

    let vectors = db.load_all_vectors_by_mode("strip")?;
    if !vectors.is_empty() {
        families.extend(crate::similarity::duplicates::find_pattern_families(
            &vectors, threshold, min_group,
        ));
    }

    let names = db.function_symbol_names()?;
    if !names.is_empty() {
        families.extend(crate::similarity::duplicates::find_name_families(
            &names, threshold, min_group,
        ));
    }

    if families.is_empty() {
        return Ok(json!({
            "families": [],
            "total": 0,
            "threshold": threshold,
            "min_group": min_group,
        }));
    }

    let sym_ids: Vec<i64> = families
        .iter()
        .flat_map(|f| &f.member_symbol_ids)
        .copied()
        .collect();
    let sym_meta = db.symbols_by_ids(&sym_ids)?;

    let family_json: Vec<serde_json::Value> = families
        .iter()
        .enumerate()
        .map(|(i, fam)| {
            let members: Vec<serde_json::Value> = fam
                .member_symbol_ids
                .iter()
                .filter_map(|&sid| {
                    sym_meta.iter().find(|s| s.id == sid).map(|s| {
                        json!({
                            "symbol": &s.qualified_name,
                            "file": &s.file_path,
                            "lines": format!("{}-{}", s.start_line, s.end_line),
                        })
                    })
                })
                .collect();
            json!({
                "family_id": i + 1,
                "detection": fam.detection_mode,
                "member_count": fam.member_symbol_ids.len(),
                "avg_similarity": (fam.avg_similarity * 1000.0).round() / 1000.0,
                "members": members,
            })
        })
        .collect();

    Ok(json!({
        "families": family_json,
        "total": families.len(),
        "threshold": threshold,
        "min_group": min_group,
    }))
}
