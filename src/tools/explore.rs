use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

use crate::db::{Db, SearchTier, SymbolRow};
use crate::error::Result;
use crate::parser::adapter::any_language_is_test_path;
use crate::tools::context::{estimate_tokens, read_line_span};
use crate::tools::explore_lexical::{self, DocFields};
use crate::tools::outline;
use crate::vocabulary;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExploreArgs {
    #[serde(default)]
    pub workspace: String,
    /// Topic to explore — symbol names, concepts, feature areas
    pub query: String,
    /// Max items to return (default 10)
    #[serde(default)]
    pub budget: Option<i64>,
    /// Drop the per-item `signature` and `doc` fields, returning the leaner
    /// pre-sutra/389 shape. Off by default: each item carries the symbol's
    /// signature and first doc line so you can usually pick the right one
    /// without a follow-up fetch.
    #[serde(default)]
    pub compact: Option<bool>,
}

fn fan_out_depth(unique_hits: usize) -> usize {
    match unique_hits {
        0 => 0,
        1..=3 => 2,
        4..=9 => 1,
        _ => 0,
    }
}

/// Below this NAME-field coverage, the query's terms never hit a symbol name on
/// the top result — only incidental body/path tokens (sutra/371).
const WEAK_STRONG_COVERAGE: f64 = 0.34;
/// Below this any-field coverage, the query's terms barely overlap the top
/// result at all.
const WEAK_COVERAGE: f64 = 0.5;

fn select_strategy(
    scores: &[f64],
    coverage: f64,
    coverage_strong: f64,
    comp_counts: &[(String, usize)],
    compact: bool,
) -> Value {
    let n = scores.len();
    // `compact` responses strip the per-item `signature`/`doc` fields, so the
    // guidance must not tell the agent to "pick by signature" it can't see.
    let pick_by_sig = if compact {
        ""
    } else {
        "pick by signature and "
    };

    if n == 0 {
        return json!({
            "action": "narrow_query",
            "rationale": "No symbols matched the query. Try a more specific or different term."
        });
    }

    if n < 3 {
        return json!({
            "action": "read_all",
            "rationale": format!("Only {} items — {}read what fits.", n, pick_by_sig)
        });
    }

    // No strong match: the query's rare, discriminating terms only weakly
    // overlap the top result — it landed on incidental body tokens, not a
    // symbol name. Suggest narrowing instead of trusting a lexical collision.
    // This is the coverage-driven replacement for the old grep-count heuristic
    // (sutra/371); the thresholds are retuned alongside the final blend in 392.
    if coverage_strong < WEAK_STRONG_COVERAGE && coverage < WEAK_COVERAGE {
        let mut rationale =
            "No strong match — the query's terms only weakly overlap the top result.".to_string();
        if !comp_counts.is_empty() {
            let suggestions: Vec<String> = comp_counts
                .iter()
                .take(3)
                .map(|(name, count)| format!("{} ({} hits)", name, count))
                .collect();
            rationale.push_str(&format!(
                " Try narrowing to a component: {}.",
                suggestions.join(", ")
            ));
        }
        let suggested_refinements: Vec<&str> = comp_counts
            .iter()
            .take(3)
            .map(|(name, _)| name.as_str())
            .collect();
        return json!({
            "action": "narrow_query",
            "rationale": rationale,
            "suggested_refinements": suggested_refinements
        });
    }

    if n >= 2 && scores[0] > 2.0 * scores[1] {
        return json!({
            "action": "read_top_n",
            "n": 1,
            "rationale": "Top result scores well above the rest — start there."
        });
    }

    if let Some((top_comp, top_count)) = comp_counts.first()
        && *top_count as f64 / n as f64 >= 0.8
    {
        return json!({
            "action": "explore_component",
            "component": top_comp,
            "rationale": format!(
                "{}% of results are in component '{}' — explore it directly.",
                (*top_count * 100) / n,
                top_comp
            )
        });
    }

    let within_2x = scores
        .iter()
        .take(3)
        .take_while(|&&s| s * 2.0 >= scores[0])
        .count();
    let read_n = within_2x.min(3);
    let rationale = if compact {
        format!("{} items found. Start with the top {} matches.", n, read_n)
    } else {
        format!(
            "{} items found. Pick by signature — start with the top {} matches.",
            n, read_n
        )
    };
    json!({
        "action": "read_top_n",
        "n": read_n,
        "rationale": rationale
    })
}

fn collect_fan_out(
    db: &Db,
    direct_hits: &[(crate::db::SymbolRow, f64)],
    max_depth: usize,
) -> Vec<(crate::db::SymbolRow, f64)> {
    if max_depth == 0 {
        return vec![];
    }

    let direct_ids: HashSet<i64> = direct_hits.iter().map(|(s, _)| s.id).collect();
    let mut visited = direct_ids.clone();
    let mut queue: VecDeque<(i64, i64, i64, i64, f64, usize)> = VecDeque::new();
    let mut fan_out_items: Vec<(crate::db::SymbolRow, f64)> = Vec::new();

    for (sym, score) in direct_hits {
        queue.push_back((sym.id, sym.file_id, sym.start_line, sym.end_line, *score, 0));
    }

    while let Some((sid, file_id, start, end, parent_score, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        let decayed = parent_score * 0.5;

        if let Ok(refs) = db.find_refs_to_symbol(sid) {
            for r in refs.iter().filter(|r| r.context_kind == "call") {
                if let Ok(Some(caller)) = db.find_enclosing_symbol(r.file_id, r.line)
                    && visited.insert(caller.id)
                {
                    let next = (
                        caller.id,
                        caller.file_id,
                        caller.start_line,
                        caller.end_line,
                        decayed,
                        depth + 1,
                    );
                    fan_out_items.push((caller, decayed));
                    queue.push_back(next);
                }
            }
        }

        if let Ok(refs) = db.find_refs_in_file(file_id) {
            for r in refs
                .iter()
                .filter(|r| r.context_kind == "call" && r.line >= start && r.line <= end)
            {
                if let Some(target_id) = r.target_symbol_id
                    && let Ok(Some(callee)) = db.symbol_by_id(target_id)
                    && visited.insert(callee.id)
                {
                    let next = (
                        callee.id,
                        callee.file_id,
                        callee.start_line,
                        callee.end_line,
                        decayed,
                        depth + 1,
                    );
                    fan_out_items.push((callee, decayed));
                    queue.push_back(next);
                }
            }
        }
    }

    fan_out_items
}

fn collect_edges(db: &Db, items: &[(crate::db::SymbolRow, f64)]) -> Vec<Value> {
    let id_set: HashSet<i64> = items.iter().map(|(s, _)| s.id).collect();
    let name_by_id: HashMap<i64, &str> = items
        .iter()
        .map(|(s, _)| (s.id, &*s.qualified_name))
        .collect();
    let mut seen = HashSet::new();
    let mut edges = Vec::new();

    for (sym, _) in items {
        if let Ok(refs) = db.find_refs_in_file(sym.file_id) {
            for r in refs.iter().filter(|r| {
                r.context_kind == "call" && r.line >= sym.start_line && r.line <= sym.end_line
            }) {
                if let Some(tid) = r.target_symbol_id
                    && id_set.contains(&tid)
                    && tid != sym.id
                {
                    let key = (sym.id, tid);
                    if seen.insert(key) {
                        edges.push(json!({
                            "from": sym.qualified_name,
                            "to": name_by_id[&tid],
                            "kind": "call",
                        }));
                    }
                }
            }
        }
    }

    edges
}

/// Average source bytes per line, used only when a span's source can't be read
/// (stale index / deleted file). The normal path reads the actual span bytes,
/// so this constant never drives the estimate for a live symbol.
const FALLBACK_BYTES_PER_LINE: usize = 40;

/// Estimate a symbol span's token cost from its source bytes, via the shared
/// char-based estimator. Degrades to a bytes-per-line approximation only when
/// the span is unknown or its source is unreadable — never `lines * 4`.
fn span_tokens(workspace_root: &Path, rel_path: &str, span: Option<(i64, i64)>) -> i64 {
    if let Some((start, end)) = span
        && let Some(src) = read_line_span(workspace_root, rel_path, start, end)
    {
        return estimate_tokens(&src) as i64;
    }
    let lines = span.map(|(s, e)| (e - s + 1).max(1) as usize).unwrap_or(10);
    (lines * FALLBACK_BYTES_PER_LINE / 4) as i64
}

/// First line of a docstring, trimmed and capped for compact display, with a
/// trailing ellipsis when the line was truncated. Returns None when the
/// docstring has no non-empty first line. Rust/Dart docstrings are stored with
/// their summary on the first line (the joined `///` lines), so the first line
/// is the natural one-glance description. sutra/389.
fn doc_line(docstring: &str) -> Option<String> {
    const MAX: usize = 120;
    let first = docstring.lines().next()?.trim();
    if first.is_empty() {
        return None;
    }
    if first.chars().count() <= MAX {
        Some(first.to_string())
    } else {
        let cut: String = first.chars().take(MAX).collect();
        Some(format!("{}…", cut.trim_end()))
    }
}

/// Attach the `signature` and `doc` presentation fields to an explore item so
/// an agent can pick the right symbol without a follow-up fetch (sutra/389).
/// No-op when `compact`. `signature` is the exact string sutra_outline renders
/// (shared via `outline::rendered_signature`). It is present on every multi-line
/// item (`lines > 1`) — carrying explicit `null` for declaration kinds the index
/// stores no signature for (e.g. structs, classes) so a missing signature is
/// distinguishable from `compact` mode, where the field is absent entirely. It is
/// omitted for one-liners (`lines <= 1`, e.g. struct fields) where it adds nothing
/// beyond the name. `doc` is the capped first line of the docstring, present only
/// when the symbol is documented.
fn enrich_item(entry: &mut Value, sym: &SymbolRow, lines: i64, compact: bool) {
    if compact {
        return;
    }
    if lines > 1 {
        entry["signature"] = json!(outline::rendered_signature(sym));
    }
    if let Some(doc) = sym.docstring.as_deref().and_then(doc_line) {
        entry["doc"] = json!(doc);
    }
}

/// FTS candidates pulled before in-process re-ranking. Generous so the scorer
/// sees enough recall for a broad query; the scored list is truncated to the
/// caller's budget afterward.
const CANDIDATE_LIMIT: i64 = 200;

/// Tokenize one symbol's fields for lexical scoring (sutra/371). `name` unions
/// the short and qualified name tokens (the highest-weight field); `body` is
/// signature + docstring — sutra stores no full body_text, so those are the
/// only body tokens available.
fn build_doc_fields(sym: &SymbolRow, path: &str) -> DocFields {
    let mut name_tokens = explore_lexical::tokenize(&sym.short_name);
    name_tokens.extend(explore_lexical::tokenize(&sym.qualified_name));

    let mut body_tokens = Vec::new();
    if let Some(sig) = sym.signature.as_deref() {
        body_tokens.extend(explore_lexical::tokenize(sig));
    }
    if let Some(doc) = sym.docstring.as_deref() {
        body_tokens.extend(explore_lexical::tokenize(doc));
    }

    DocFields {
        name: explore_lexical::counts(name_tokens),
        path: explore_lexical::counts(explore_lexical::tokenize(path)),
        body: explore_lexical::counts(body_tokens),
    }
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    query: &str,
    budget: i64,
    compact: bool,
) -> Result<Value> {
    // Priority 0: alias resolution — check .sutra/aliases.toml, component names, anchor names
    // Filter out orphan matches (targets that no longer exist) and fall through if nothing valid
    let alias_matches = vocabulary::resolve(db, query).unwrap_or_default();
    let valid_aliases: Vec<_> = alias_matches.iter().filter(|m| !m.orphan).collect();
    if !valid_aliases.is_empty() {
        let budget = budget.max(1) as usize;
        let mut items = Vec::new();
        for m in &valid_aliases {
            if items.len() >= budget {
                break;
            }
            for loc in &m.locations {
                if items.len() >= budget {
                    break;
                }
                let span = match (loc.start_line, loc.end_line) {
                    (Some(s), Some(e)) => Some((s, e)),
                    _ => None,
                };
                let lines = span.map(|(s, e)| e - s + 1).unwrap_or(10);
                let estimated_tokens = span_tokens(workspace_root, &loc.path, span);
                let fetch = if matches!(
                    m.target_kind.as_str(),
                    "symbol" | "function" | "struct" | "method"
                ) {
                    format!("sutra_symbol(symbol='{}')", m.target_ref)
                } else if m.target_kind == "component" {
                    format!("sutra_map(workspace='...', component='{}')", m.target_ref)
                } else {
                    format!("sutra_outline(path='{}')", loc.path)
                };
                // No signature/doc enrichment here (sutra/389): a vocabulary
                // match carries a bare `target_ref` — often a short name, a
                // group, a component, or a doc path — with no SymbolRow, and no
                // location-keyed symbol lookup to disambiguate a colliding
                // short name against. An exact alias hit already ships a
                // precise `fetch`, so the "which item answers my question?"
                // problem the fields solve doesn't arise here.
                items.push(json!({
                    "symbol": &m.target_ref,
                    "file": &loc.path,
                    "kind": &m.target_kind,
                    "lines": lines,
                    "component": &m.component_id,
                    "reason": format!("alias:{}", m.source),
                    "estimated_tokens": estimated_tokens,
                    "fetch": fetch,
                }));
            }
        }
        if !items.is_empty() {
            let total_tokens: i64 = items
                .iter()
                .filter_map(|i| i["estimated_tokens"].as_i64())
                .sum();
            let n = items.len();
            return Ok(json!({
                "items": items,
                "edges": [],
                "strategy": {
                    "action": if n <= 3 { "read_all" } else { "read_top_n" },
                    "n": n.min(3),
                    "rationale": format!("Alias/vocabulary match for '{}' — {} location(s) found.", query, n),
                },
                "summary": {
                    "total_items": n,
                    "direct_matches": n,
                    "fan_out_items": 0,
                    "components_touched": items.iter().filter_map(|i| i["component"].as_str()).collect::<HashSet<_>>().len(),
                    "total_estimated_tokens": total_tokens,
                },
            }));
        }
        // All alias matches were orphans or had no locations — fall through to symbol search
    }

    // Qualified-name detection: query containing :: falls through to exact lookup
    if query.contains("::") {
        let (symbols, _tier) = db.find_symbols_by_name_tiered(query, None, 1)?;
        if symbols.is_empty() {
            return Ok(json!({
                "items": [],
                "edges": [],
                "strategy": {
                    "action": "narrow_query",
                    "rationale": format!("No symbol matching '{}' found. Check the qualified name.", query)
                },
                "summary": {
                    "total_items": 0,
                    "direct_matches": 0,
                    "fan_out_items": 0,
                    "components_touched": 0,
                    "total_estimated_tokens": 0
                }
            }));
        }
        let sym = &symbols[0];
        let (item, estimated_tokens) = direct_match_item(db, workspace_root, sym, compact);
        return Ok(json!({
            "items": [item],
            "edges": [],
            "strategy": {
                "action": "read_top_n",
                "n": 1,
                "rationale": "Qualified symbol lookup — read it directly."
            },
            "summary": {
                "total_items": 1,
                "direct_matches": 1,
                "fan_out_items": 0,
                "components_touched": 1,
                "total_estimated_tokens": estimated_tokens
            }
        }));
    }

    let budget = budget.max(1) as usize;

    // ── Lexical stage (sutra/371) ──────────────────────────────────────────
    // Tokenize the query and weight each distinct token by its IDF over the
    // whole symbol corpus, so a token in nearly every symbol (`handle`, `new`)
    // weighs ~0 and a rare, discriminating one dominates. An empty IDF map means
    // the query had no searchable terms after tokenization (all stop words /
    // single chars).
    let corpus_n = db.symbol_count().unwrap_or(0);
    let idf = explore_lexical::build_idf(query, corpus_n, |t| db.fts_doc_frequency(t).unwrap_or(0));
    if idf.is_empty() {
        // The query tokenized to nothing searchable — every term was a stop word
        // or single char (e.g. a bare `get` / `set`). The lexical stage can't
        // run, but a symbol *named* exactly that should still surface, as it did
        // before sutra/371 removed expand_patterns. Fall back to the exact
        // short/qualified-name tier only — never the FTS prefix tier — so a
        // stop-word query can't flood with getFoo/setBar (sutra/394).
        return exact_name_fallback(db, workspace_root, query, budget, compact);
    }

    // Candidate retrieval: a prefix OR over the query tokens, scoped to the
    // lex_tokens column. lex_tokens holds every searchable field (name,
    // qualified name, docstring, signature) pre-split by the shared tokenizer,
    // so an interior camelCase component like `context` in `RequestContext` is
    // reachable — SQLite's unicode61 stores such names whole in the raw columns,
    // where a `context*` prefix never matches (sutra/394). Scoping to lex_tokens
    // also leaves find/lookup (which query the raw name columns) untouched. This
    // is a recall net; the IDF/BM25 scorer re-ranks it below.
    let inner = idf
        .keys()
        .map(|t| format!("\"{}\"*", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let match_expr = format!("{{lex_tokens}} : ({inner})");
    let candidates = db.fts_candidates(&match_expr, CANDIDATE_LIMIT)?;

    // Fetch FileRows for path tokenization + component resolution.
    let unique_file_ids: Vec<i64> = candidates
        .iter()
        .map(|s| s.file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut file_map: HashMap<i64, crate::db::FileRow> = unique_file_ids
        .iter()
        .filter_map(|&fid| db.file_by_id(fid).ok().flatten().map(|f| (fid, f)))
        .collect();

    // Tokenize each candidate's fields once, and settle its query-aware test
    // de-rank factor (a query that asks about tests keeps them at full weight).
    let query_wants_tests = explore_lexical::wants_tests(query);
    let docs: Vec<(SymbolRow, DocFields, f64)> = candidates
        .into_iter()
        .map(|sym| {
            let path = file_map.get(&sym.file_id).map(|f| &*f.path).unwrap_or("");
            let test_factor = if !query_wants_tests && any_language_is_test_path(path) {
                explore_lexical::TEST_RANK_PENALTY
            } else {
                1.0
            };
            let fields = build_doc_fields(&sym, path);
            (sym, fields, test_factor)
        })
        .collect();

    // BM25 length prior: the corpus-average body length, approximated over the
    // candidate set (a full-corpus average would cost a tokenize of every
    // symbol per query). Relative ordering within candidates is what matters.
    let avgdl = if docs.is_empty() {
        1.0
    } else {
        docs.iter()
            .map(|(_, f, _)| explore_lexical::body_len(f))
            .sum::<f64>()
            / docs.len() as f64
    };

    // Raw lexical pass: keep positive hits with their coverage + test factor.
    struct RawHit {
        sym: SymbolRow,
        lexical: f64,
        test_factor: f64,
        coverage: f64,
        coverage_strong: f64,
    }
    let mut raw: Vec<RawHit> = Vec::with_capacity(docs.len());
    for (sym, fields, test_factor) in docs {
        let s = explore_lexical::score_doc(&idf, &fields, avgdl);
        if s.score > 0.0 {
            raw.push(RawHit {
                sym,
                lexical: s.score,
                test_factor,
                coverage: s.coverage,
                coverage_strong: s.coverage_strong,
            });
        }
    }

    // Normalization maxima over the candidate set, then blend the normalized
    // lexical score with the structural (global pagerank) prior and apply the
    // test de-rank to the blend (sutra/371; query-personalized re-rank is 372).
    let max_lex = raw.iter().map(|h| h.lexical).fold(0.0_f64, f64::max);
    let max_lex = if max_lex > 0.0 { max_lex } else { 1.0 };
    let max_pagerank = raw
        .iter()
        .filter_map(|h| h.sym.pagerank)
        .fold(0.0_f64, f64::max);
    let max_pagerank = if max_pagerank > 0.0 {
        max_pagerank
    } else {
        1.0
    };

    let mut coverage_by_id: HashMap<i64, (f64, f64)> = HashMap::new();
    let mut scored: Vec<(SymbolRow, f64)> = Vec::with_capacity(raw.len());
    for h in raw {
        let blended = explore_lexical::blend(
            h.lexical / max_lex,
            h.sym.pagerank.unwrap_or(0.0) / max_pagerank,
            h.test_factor,
        );
        coverage_by_id.insert(h.sym.id, (h.coverage, h.coverage_strong));
        scored.push((h.sym, blended));
    }

    let total_hits = scored.len();
    let direct_ids: HashSet<i64> = scored.iter().map(|(s, _)| s.id).collect();
    let depth = fan_out_depth(total_hits);
    let fan_out = collect_fan_out(db, &scored, depth);
    scored.extend(fan_out);
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.qualified_name.cmp(&b.0.qualified_name))
    });
    scored.truncate(budget);

    // Match-strength of the top hit — the signal the strategy hint reads.
    let (top_coverage, top_coverage_strong) = scored
        .first()
        .and_then(|(s, _)| coverage_by_id.get(&s.id).copied())
        .unwrap_or((0.0, 0.0));

    // Extend file_map with any new files from fan-out items
    for (sym, _) in &scored {
        if let std::collections::hash_map::Entry::Vacant(e) = file_map.entry(sym.file_id)
            && let Ok(Some(f)) = db.file_by_id(sym.file_id)
        {
            e.insert(f);
        }
    }

    let edges = collect_edges(db, &scored);

    let budgeted_file_ids: Vec<i64> = scored
        .iter()
        .map(|(s, _)| s.file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let component_map = db
        .component_names_by_file_ids(&budgeted_file_ids)
        .unwrap_or_default();

    // Compute per-component item counts for strategy selection
    let mut comp_count_map: HashMap<String, usize> = HashMap::new();
    for (sym, _) in &scored {
        if let Some(name) = component_map.get(&sym.file_id) {
            *comp_count_map.entry(name.clone()).or_default() += 1;
        }
    }
    let mut comp_counts: Vec<(String, usize)> = comp_count_map.into_iter().collect();
    comp_counts.sort_by_key(|b| std::cmp::Reverse(b.1));

    let scores: Vec<f64> = scored.iter().map(|(_, s)| *s).collect();

    let direct_count = scored
        .iter()
        .filter(|(s, _)| direct_ids.contains(&s.id))
        .count() as i64;
    let fan_out_count = scored.len() as i64 - direct_count;

    let items: Vec<Value> = scored
        .iter()
        .map(|(sym, _score)| {
            let file_path = file_map
                .get(&sym.file_id)
                .map(|f| Arc::clone(&f.path))
                .unwrap_or_default();
            let component = component_map.get(&sym.file_id);
            let lines = sym.end_line - sym.start_line + 1;
            let estimated_tokens = span_tokens(
                workspace_root,
                &file_path,
                Some((sym.start_line, sym.end_line)),
            );
            let reason = if direct_ids.contains(&sym.id) {
                "direct_match"
            } else {
                "fan_out"
            };
            let mut entry = json!({
                "symbol": sym.qualified_name,
                "file": file_path,
                "kind": sym.kind,
                "lines": lines,
                "component": component,
                "reason": reason,
                "estimated_tokens": estimated_tokens,
                "fetch": format!("sutra_symbol(symbol='{}')", sym.qualified_name),
            });
            enrich_item(&mut entry, sym, lines, compact);
            entry
        })
        .collect();

    let total_tokens: i64 = items
        .iter()
        .filter_map(|i| i["estimated_tokens"].as_i64())
        .sum();

    let components_touched = items
        .iter()
        .filter_map(|i| i["component"].as_str())
        .collect::<HashSet<_>>()
        .len();

    Ok(json!({
        "items": items,
        "edges": edges,
        "strategy": select_strategy(&scores, top_coverage, top_coverage_strong, &comp_counts, compact),
        "summary": {
            "total_items": items.len(),
            "direct_matches": direct_count,
            "fan_out_items": fan_out_count,
            "components_touched": components_touched,
            "total_estimated_tokens": total_tokens,
            "coverage": top_coverage,
            "coverage_strong": top_coverage_strong,
        },
    }))
}

/// Build a single "direct match" item (symbol, file, span, token cost, fetch
/// hint) from a resolved `SymbolRow`, enriched per `compact`. Shared by the
/// qualified-name (`::`) lookup and the stop-word exact-name fallback so both
/// emit an identical item shape. Returns the item and its estimated token cost.
fn direct_match_item(
    db: &Db,
    workspace_root: &Path,
    sym: &SymbolRow,
    compact: bool,
) -> (Value, i64) {
    let file_path = db
        .file_by_id(sym.file_id)
        .ok()
        .flatten()
        .map(|f| Arc::clone(&f.path))
        .unwrap_or_default();
    let component_map = db
        .component_names_by_file_ids(&[sym.file_id])
        .unwrap_or_default();
    let component = component_map.get(&sym.file_id);
    let lines = sym.end_line - sym.start_line + 1;
    let estimated_tokens = span_tokens(
        workspace_root,
        &file_path,
        Some((sym.start_line, sym.end_line)),
    );
    let mut item = json!({
        "symbol": sym.qualified_name,
        "file": file_path,
        "kind": sym.kind,
        "lines": lines,
        "component": component,
        "reason": "direct_match",
        "estimated_tokens": estimated_tokens,
        "fetch": format!("sutra_symbol(symbol='{}')", sym.qualified_name),
    });
    enrich_item(&mut item, sym, lines, compact);
    (item, estimated_tokens)
}

/// Exact short/qualified-name lookup for a query the lexical stage can't handle
/// because it tokenized to nothing (all stop words or single chars, e.g. a bare
/// `get` / `set`). Returns items for symbols named exactly `query`, or the empty
/// lexical result when there is no exact match. Takes only the Exact tier — never
/// the FTS prefix tier — so a stop-word query can't flood with `getFoo`/`setBar`.
/// Restores the pre-sutra/371 recall for bare identifier queries without
/// resurrecting `expand_patterns` (sutra/394).
fn exact_name_fallback(
    db: &Db,
    workspace_root: &Path,
    query: &str,
    budget: usize,
    compact: bool,
) -> Result<Value> {
    let (symbols, tier) = db.find_symbols_by_name_tiered(query, None, budget as i64)?;
    if tier != SearchTier::Exact || symbols.is_empty() {
        return Ok(empty_lexical_result(query));
    }
    let mut items = Vec::with_capacity(symbols.len().min(budget));
    let mut total_tokens = 0i64;
    for sym in symbols.iter().take(budget) {
        let (item, est) = direct_match_item(db, workspace_root, sym, compact);
        total_tokens += est;
        items.push(item);
    }
    let n = items.len();
    let components_touched = items
        .iter()
        .filter_map(|i| i["component"].as_str())
        .collect::<HashSet<_>>()
        .len();
    Ok(json!({
        "items": items,
        "edges": [],
        "strategy": {
            "action": if n <= 3 { "read_all" } else { "read_top_n" },
            "n": n.min(3),
            "rationale": format!(
                "Exact name match for '{query}' — {n} symbol(s) named that ('{query}' is too short/common to search lexically)."
            ),
        },
        "summary": {
            "total_items": n,
            "direct_matches": n,
            "fan_out_items": 0,
            "components_touched": components_touched,
            "total_estimated_tokens": total_tokens,
            "coverage": 1.0,
            "coverage_strong": 1.0,
        },
    }))
}

/// The empty result for a query that tokenizes to nothing searchable — same
/// shape as a zero-hit lexical result, with the coverage signals at 0.
fn empty_lexical_result(query: &str) -> Value {
    json!({
        "items": [],
        "edges": [],
        "strategy": {
            "action": "narrow_query",
            "rationale": format!(
                "Query '{query}' has no searchable terms after tokenization — try a specific identifier or a longer word."
            ),
        },
        "summary": {
            "total_items": 0,
            "direct_matches": 0,
            "fan_out_items": 0,
            "components_touched": 0,
            "total_estimated_tokens": 0,
            "coverage": 0.0,
            "coverage_strong": 0.0,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_line_takes_first_line_only() {
        assert_eq!(
            doc_line("Summary line.\nSecond paragraph with more detail."),
            Some("Summary line.".to_string())
        );
    }

    #[test]
    fn doc_line_trims_and_keeps_short_line_whole() {
        assert_eq!(
            doc_line("  Does the thing.  "),
            Some("Does the thing.".to_string())
        );
    }

    #[test]
    fn doc_line_caps_long_first_line_with_ellipsis() {
        let long = "x".repeat(200);
        let out = doc_line(&long).unwrap();
        // 120 chars kept + one ellipsis char.
        assert_eq!(out.chars().count(), 121);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn doc_line_empty_or_blank_is_none() {
        assert_eq!(doc_line(""), None);
        assert_eq!(doc_line("   \n"), None);
    }

    #[test]
    fn strategy_zero_items() {
        let s = select_strategy(&[], 0.0, 0.0, &[], false);
        assert_eq!(s["action"], "narrow_query");
    }

    #[test]
    fn strategy_read_all_few_items() {
        let s = select_strategy(&[0.8, 0.5], 0.9, 0.9, &[], false);
        assert_eq!(s["action"], "read_all");
        assert!(
            s["rationale"]
                .as_str()
                .unwrap()
                .contains("pick by signature"),
            "non-compact read_all should mention signature"
        );
        // compact mode strips signatures, so the rationale must not reference them
        let sc = select_strategy(&[0.8, 0.5], 0.9, 0.9, &[], true);
        assert!(!sc["rationale"].as_str().unwrap().contains("signature"));
    }

    #[test]
    fn strategy_narrow_on_weak_coverage() {
        // sutra/371: the hint reads coverage, not a grep-count. Many strong-
        // scoring hits, but the query's rare terms barely overlap the top result
        // (coverage_strong 0.1, coverage 0.2) → narrow with component hints.
        let scores = vec![0.9, 0.85, 0.8, 0.7, 0.6];
        let comps = vec![
            ("parser".to_string(), 2),
            ("db".to_string(), 2),
            ("tools".to_string(), 1),
        ];
        let s = select_strategy(&scores, 0.2, 0.1, &comps, false);
        assert_eq!(s["action"], "narrow_query");
        assert!(s["suggested_refinements"].is_array());
        let refs = s["suggested_refinements"].as_array().unwrap();
        assert_eq!(refs[0], "parser");
    }

    #[test]
    fn strategy_strong_coverage_not_narrowed() {
        // Same diffuse score shape, but a strong name-field match: the narrow
        // branch is skipped — proof the decision consults coverage, not counts.
        let scores = vec![0.9, 0.85, 0.8, 0.7, 0.6];
        let s = select_strategy(&scores, 1.0, 1.0, &[], false);
        assert_ne!(s["action"], "narrow_query");
    }

    #[test]
    fn strategy_read_top_1_dominant() {
        // Top score > 2× second, with a strong match so coverage doesn't narrow.
        let scores = vec![0.9, 0.3, 0.2, 0.1];
        let s = select_strategy(&scores, 1.0, 1.0, &[], false);
        assert_eq!(s["action"], "read_top_n");
        assert_eq!(s["n"], 1);
    }

    #[test]
    fn strategy_explore_component() {
        // 8 of 10 items in "parser" component → 80%
        let scores = vec![0.7, 0.6, 0.5, 0.5, 0.4, 0.4, 0.3, 0.3, 0.2, 0.2];
        let comps = vec![("parser".to_string(), 8), ("db".to_string(), 2)];
        let s = select_strategy(&scores, 1.0, 1.0, &comps, false);
        assert_eq!(s["action"], "explore_component");
        assert_eq!(s["component"], "parser");
    }

    #[test]
    fn strategy_read_top_n_cluster() {
        // Top 3 within 2× of each other, no single dominant, mixed components
        let scores = vec![0.8, 0.6, 0.5, 0.3, 0.2];
        let comps = vec![
            ("tools".to_string(), 2),
            ("db".to_string(), 2),
            ("parser".to_string(), 1),
        ];
        let s = select_strategy(&scores, 1.0, 1.0, &comps, false);
        assert_eq!(s["action"], "read_top_n");
        // 0.6 * 2 = 1.2 >= 0.8 ✓, 0.5 * 2 = 1.0 >= 0.8 ✓ → 3 within 2×
        assert_eq!(s["n"], 3);
        assert!(
            s["rationale"]
                .as_str()
                .unwrap()
                .contains("Pick by signature")
        );
        // compact read_top_n drops the signature guidance
        let sc = select_strategy(&scores, 1.0, 1.0, &comps, true);
        assert!(!sc["rationale"].as_str().unwrap().contains("signature"));
    }

    #[test]
    fn strategy_read_top_2_when_third_drops() {
        // Top 2 close, third drops off
        let scores = vec![0.8, 0.7, 0.3, 0.2];
        let s = select_strategy(&scores, 1.0, 1.0, &[], false);
        assert_eq!(s["action"], "read_top_n");
        // 0.7 * 2 = 1.4 >= 0.8 ✓, 0.3 * 2 = 0.6 < 0.8 ✗ → 2 within 2×
        assert_eq!(s["n"], 2);
    }

    #[test]
    fn fan_out_depth_thresholds() {
        assert_eq!(fan_out_depth(0), 0);
        assert_eq!(fan_out_depth(1), 2);
        assert_eq!(fan_out_depth(3), 2);
        assert_eq!(fan_out_depth(4), 1);
        assert_eq!(fan_out_depth(9), 1);
        assert_eq!(fan_out_depth(10), 0);
        assert_eq!(fan_out_depth(100), 0);
    }

    #[test]
    fn span_tokens_estimates_from_bytes_not_lines() {
        let dir = tempfile::tempdir().unwrap();
        let rel = "sample.rs";
        // Three dense lines — ~50 chars each, far more than the 4 tokens/line
        // the old `lines * 4` formula assumed.
        let content = "let alpha = compute_something(beta, gamma, delta);\n\
                       let result = alpha.transform().collect::<Vec<_>>();\n\
                       return result.into_iter().map(|x| x + 1).sum();";
        std::fs::write(dir.path().join(rel), content).unwrap();
        let lines = content.lines().count() as i64;

        let est = span_tokens(dir.path(), rel, Some((1, lines)));

        // Byte-based: matches the shared char estimator on the actual span source.
        assert_eq!(est, estimate_tokens(content) as i64);
        // Within ±20% of the chars/4 proxy for a real tokenizer count.
        let proxy = content.chars().count() as i64 / 4;
        assert!(
            (est - proxy).abs() * 5 <= proxy,
            "estimate {est} should be within ±20% of the chars/4 count ({proxy})"
        );
        // ...and well above the old `lines * 4` undercount.
        assert!(
            est > lines * 4,
            "estimate {est} should exceed lines*4 = {}",
            lines * 4
        );
    }

    #[test]
    fn span_tokens_fallback_is_not_lines_times_four() {
        let dir = tempfile::tempdir().unwrap();
        // Unreadable span: degrade to bytes-per-line, never `lines * 4`.
        let est = span_tokens(dir.path(), "missing.rs", Some((1, 10)));
        assert_eq!(est, (10 * FALLBACK_BYTES_PER_LINE / 4) as i64);
        assert!(est > 10 * 4);
    }
}
