use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::db::Db;
use crate::error::Result;

const DEFINITION_KINDS: &[&str] = &["function", "struct", "trait", "impl", "method", "enum"];

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ExploreArgs {
    pub workspace: String,
    /// Topic to explore — symbol names, concepts, feature areas
    pub query: String,
    /// Max items to return (default 10)
    #[serde(default)]
    pub budget: Option<i64>,
}

fn expand_patterns(query: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut patterns = Vec::new();
    let mut push = |s: String| {
        if seen.insert(s.clone()) {
            patterns.push(s);
        }
    };

    push(query.to_string());

    // Split on whitespace: "import parsing" → join as "import_parsing", plus individual words
    let ws_words: Vec<&str> = query.split_whitespace().collect();
    if ws_words.len() > 1 {
        push(ws_words.join("_"));
        for word in &ws_words {
            if word.len() >= 3 {
                push(word.to_string());
            }
        }
    }

    // Split on underscores: "parse_imports" → individual segments
    let us_words: Vec<&str> = query.split('_').collect();
    if us_words.len() > 1 {
        for word in &us_words {
            if word.len() >= 3 {
                push(word.to_string());
            }
        }
    }

    // CamelCase variant from multi-word queries: "import parsing" → "ImportParsing" / "ImportPars"
    let camel_words: Vec<&str> = query
        .split(|c: char| c == '_' || c == ' ')
        .filter(|w| !w.is_empty())
        .collect();
    if camel_words.len() > 1 {
        let title_case = |w: &str| -> String {
            let mut chars = w.chars();
            match chars.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
            }
        };
        let full: String = camel_words.iter().map(|w| title_case(w)).collect();
        push(full);
        if let Some(last) = camel_words.last() {
            if last.len() > 4 {
                let mut truncated: String = camel_words[..camel_words.len() - 1]
                    .iter()
                    .map(|w| title_case(w))
                    .collect();
                let trunc_last: String = last.chars().take(4).collect();
                truncated.push_str(&title_case(&trunc_last));
                push(truncated);
            }
        }
    }

    patterns
}

fn fan_out_depth(unique_hits: usize) -> usize {
    match unique_hits {
        0 => 0,
        1..=3 => 2,
        4..=9 => 1,
        _ => 0,
    }
}

fn select_strategy(
    scores: &[f64],
    total_grep_hits: usize,
    comp_counts: &[(String, usize)],
) -> Value {
    let n = scores.len();

    if n == 0 {
        return json!({
            "action": "narrow_query",
            "rationale": "No symbols matched the query. Try a more specific or different term."
        });
    }

    if n < 3 {
        return json!({
            "action": "read_all",
            "rationale": format!("Only {} items — read them all.", n)
        });
    }

    if total_grep_hits >= 10 && scores[0] < 0.4 {
        let mut rationale = format!(
            "{} hits with no strong match — query is too broad.",
            total_grep_hits
        );
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

    if let Some((top_comp, top_count)) = comp_counts.first() {
        if *top_count as f64 / n as f64 >= 0.8 {
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
    }

    let within_2x = scores
        .iter()
        .take(3)
        .take_while(|&&s| s * 2.0 >= scores[0])
        .count();
    let read_n = within_2x.min(3);
    json!({
        "action": "read_top_n",
        "n": read_n,
        "rationale": format!("{} items found. Start with the top {} matches.", n, read_n)
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
                if let Ok(Some(caller)) = db.find_enclosing_symbol(r.file_id, r.line) {
                    if visited.insert(caller.id) {
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
        }

        if let Ok(refs) = db.find_refs_in_file(file_id) {
            for r in refs
                .iter()
                .filter(|r| r.context_kind == "call" && r.line >= start && r.line <= end)
            {
                if let Some(target_id) = r.target_symbol_id {
                    if let Ok(Some(callee)) = db.symbol_by_id(target_id) {
                        if visited.insert(callee.id) {
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
                if let Some(tid) = r.target_symbol_id {
                    if id_set.contains(&tid) && tid != sym.id {
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
    }

    edges
}

pub fn handle(db: &Db, query: &str, budget: i64) -> Result<Value> {
    // Qualified-name detection: query containing :: falls through to sutra_find behavior
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
        let file_path = db
            .file_by_id(sym.file_id)
            .ok()
            .flatten()
            .map(|f| f.path.clone())
            .unwrap_or_default();
        let file_ids = vec![sym.file_id];
        let component_map = db
            .component_names_by_file_ids(&file_ids)
            .unwrap_or_default();
        let component = component_map.get(&sym.file_id);
        let lines = sym.end_line - sym.start_line + 1;
        return Ok(json!({
            "items": [{
                "symbol": sym.qualified_name,
                "file": file_path,
                "kind": sym.kind,
                "lines": lines,
                "component": component,
                "reason": "direct_match",
                "estimated_tokens": lines * 4,
                "fetch": format!("sutra_read(symbol='{}')", sym.qualified_name),
            }],
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
                "total_estimated_tokens": lines * 4
            }
        }));
    }

    let patterns = expand_patterns(query);

    let mut hits: HashMap<i64, (crate::db::SymbolRow, usize)> = HashMap::new();
    for pattern in &patterns {
        let (symbols, _tier) = db.find_symbols_by_name_tiered(pattern, None, 50)?;
        for sym in symbols {
            hits.entry(sym.id)
                .and_modify(|(_, count)| *count += 1)
                .or_insert((sym, 1));
        }
    }

    let budget = budget.max(1) as usize;
    let total_hits = hits.len();

    // Fetch FileRows for structural importance signals
    let unique_file_ids: Vec<i64> = hits
        .values()
        .map(|(s, _)| s.file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let mut file_map: HashMap<i64, crate::db::FileRow> = unique_file_ids
        .iter()
        .filter_map(|&fid| db.file_by_id(fid).ok().flatten().map(|f| (fid, f)))
        .collect();

    // Compute normalization maxima
    let max_match_density = hits.values().map(|(_, c)| *c).max().unwrap_or(1) as f64;
    let max_structural: f64 = hits
        .values()
        .map(|(sym, _)| {
            file_map
                .get(&sym.file_id)
                .map(|f| (f.fan_in_files + f.blast_radius) as f64)
                .unwrap_or(0.0)
        })
        .fold(0.0_f64, f64::max);
    let max_structural = if max_structural > 0.0 {
        max_structural
    } else {
        1.0
    };

    // Score each hit with the 3-signal weighted formula
    let mut scored: Vec<(crate::db::SymbolRow, f64)> = hits
        .into_values()
        .map(|(sym, match_count)| {
            let match_density_norm = match_count as f64 / max_match_density;
            let structural_norm = file_map
                .get(&sym.file_id)
                .map(|f| (f.fan_in_files + f.blast_radius) as f64 / max_structural)
                .unwrap_or(0.0);
            let def_priority = if DEFINITION_KINDS.contains(&&*sym.kind) {
                1.0
            } else {
                0.0
            };
            let score = match_density_norm * 0.5 + structural_norm * 0.3 + def_priority * 0.2;
            (sym, score)
        })
        .collect();

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

    // Extend file_map with any new files from fan-out items
    for (sym, _) in &scored {
        if !file_map.contains_key(&sym.file_id) {
            if let Ok(Some(f)) = db.file_by_id(sym.file_id) {
                file_map.insert(sym.file_id, f);
            }
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
    comp_counts.sort_by(|a, b| b.1.cmp(&a.1));

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
                .map(|f| f.path.clone())
                .unwrap_or_default();
            let component = component_map.get(&sym.file_id);
            let lines = sym.end_line - sym.start_line + 1;
            let reason = if direct_ids.contains(&sym.id) {
                "direct_match"
            } else {
                "fan_out"
            };
            json!({
                "symbol": sym.qualified_name,
                "file": file_path,
                "kind": sym.kind,
                "lines": lines,
                "component": component,
                "reason": reason,
                "estimated_tokens": lines * 4,
                "fetch": format!("sutra_read(symbol='{}')", sym.qualified_name),
            })
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
        "strategy": select_strategy(&scores, total_hits, &comp_counts),
        "summary": {
            "total_items": items.len(),
            "direct_matches": direct_count,
            "fan_out_items": fan_out_count,
            "components_touched": components_touched,
            "total_estimated_tokens": total_tokens,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_single_word() {
        let patterns = expand_patterns("import");
        assert_eq!(patterns, vec!["import"]);
    }

    #[test]
    fn expand_multi_word() {
        let patterns = expand_patterns("import parsing");
        assert!(patterns.contains(&"import parsing".to_string()));
        assert!(patterns.contains(&"import_parsing".to_string()));
        assert!(patterns.contains(&"import".to_string()));
        assert!(patterns.contains(&"parsing".to_string()));
    }

    #[test]
    fn expand_snake_case_splits() {
        let patterns = expand_patterns("parse_imports");
        assert!(patterns.contains(&"parse_imports".to_string()));
        assert!(patterns.contains(&"parse".to_string()));
        assert!(patterns.contains(&"imports".to_string()));
    }

    #[test]
    fn expand_skips_short_words() {
        let patterns = expand_patterns("do it now");
        assert!(patterns.contains(&"do it now".to_string()));
        assert!(patterns.contains(&"do_it_now".to_string()));
        assert!(patterns.contains(&"now".to_string()));
        assert!(!patterns.contains(&"do".to_string()));
        assert!(!patterns.contains(&"it".to_string()));
    }

    #[test]
    fn expand_deduplicates() {
        let patterns = expand_patterns("foo bar");
        let count = patterns.iter().filter(|p| p.as_str() == "foo").count();
        assert_eq!(count, 1, "no duplicates");
    }

    #[test]
    fn expand_empty_string() {
        let patterns = expand_patterns("");
        assert_eq!(patterns, vec![""]);
    }

    #[test]
    fn expand_camel_case_from_spaces() {
        let patterns = expand_patterns("import parsing");
        assert!(patterns.contains(&"ImportParsing".to_string()));
        assert!(patterns.contains(&"ImportPars".to_string()));
    }

    #[test]
    fn expand_camel_case_from_underscores() {
        let patterns = expand_patterns("parse_imports");
        assert!(patterns.contains(&"ParseImports".to_string()));
        assert!(patterns.contains(&"ParseImpo".to_string()));
    }

    #[test]
    fn expand_camel_case_no_truncation_for_short_last_word() {
        let patterns = expand_patterns("get foo");
        assert!(patterns.contains(&"GetFoo".to_string()));
        // "foo" is only 3 chars, no truncated variant
        assert!(!patterns.iter().any(|p| p.starts_with("Get")
            && p != "GetFoo"
            && p.chars().next().unwrap().is_uppercase()));
    }

    #[test]
    fn strategy_zero_items() {
        let s = select_strategy(&[], 0, &[]);
        assert_eq!(s["action"], "narrow_query");
    }

    #[test]
    fn strategy_read_all_few_items() {
        let s = select_strategy(&[0.8, 0.5], 2, &[]);
        assert_eq!(s["action"], "read_all");
    }

    #[test]
    fn strategy_narrow_query_diffuse() {
        // 12 grep hits, weak top score, spread across components
        let scores = vec![0.3, 0.28, 0.25, 0.2, 0.18];
        let comps = vec![
            ("parser".to_string(), 2),
            ("db".to_string(), 2),
            ("tools".to_string(), 1),
        ];
        let s = select_strategy(&scores, 12, &comps);
        assert_eq!(s["action"], "narrow_query");
        assert!(s["suggested_refinements"].is_array());
        let refs = s["suggested_refinements"].as_array().unwrap();
        assert_eq!(refs[0], "parser");
    }

    #[test]
    fn strategy_read_top_1_dominant() {
        // Top score > 2× second
        let scores = vec![0.9, 0.3, 0.2, 0.1];
        let s = select_strategy(&scores, 5, &[]);
        assert_eq!(s["action"], "read_top_n");
        assert_eq!(s["n"], 1);
    }

    #[test]
    fn strategy_explore_component() {
        // 8 of 10 items in "parser" component → 80%
        let scores = vec![0.7, 0.6, 0.5, 0.5, 0.4, 0.4, 0.3, 0.3, 0.2, 0.2];
        let comps = vec![("parser".to_string(), 8), ("db".to_string(), 2)];
        let s = select_strategy(&scores, 7, &comps);
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
        let s = select_strategy(&scores, 5, &comps);
        assert_eq!(s["action"], "read_top_n");
        // 0.6 * 2 = 1.2 >= 0.8 ✓, 0.5 * 2 = 1.0 >= 0.8 ✓ → 3 within 2×
        assert_eq!(s["n"], 3);
    }

    #[test]
    fn strategy_read_top_2_when_third_drops() {
        // Top 2 close, third drops off
        let scores = vec![0.8, 0.7, 0.3, 0.2];
        let s = select_strategy(&scores, 4, &[]);
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
}
