use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};

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

    patterns
}

fn select_strategy(total_hits: usize, returned: usize) -> Value {
    if total_hits == 0 {
        json!({
            "action": "narrow_query",
            "rationale": "No symbols matched the query. Try a more specific or different term."
        })
    } else if total_hits <= 3 {
        json!({
            "action": "read_all",
            "rationale": format!("Only {} items — read them all.", total_hits)
        })
    } else if total_hits >= 10 {
        json!({
            "action": "narrow_query",
            "rationale": format!("{} hits — query is too broad. Consider narrowing.", total_hits)
        })
    } else {
        json!({
            "action": "read_top_n",
            "n": std::cmp::min(3, returned),
            "rationale": format!("{} items found. Start with the top matches.", returned)
        })
    }
}

pub fn handle(db: &Db, query: &str, budget: i64) -> Result<Value> {
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
    let file_map: HashMap<i64, crate::db::FileRow> = unique_file_ids
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
            let def_priority = if DEFINITION_KINDS.contains(&sym.kind.as_str()) {
                1.0
            } else {
                0.0
            };
            let score = match_density_norm * 0.5 + structural_norm * 0.3 + def_priority * 0.2;
            (sym, score)
        })
        .collect();

    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.qualified_name.cmp(&b.0.qualified_name))
    });
    scored.truncate(budget);

    // Component lookup for budgeted items
    let budgeted_file_ids: Vec<i64> = scored
        .iter()
        .map(|(s, _)| s.file_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let component_map = db
        .component_names_by_file_ids(&budgeted_file_ids)
        .unwrap_or_default();

    let items: Vec<Value> = scored
        .iter()
        .map(|(sym, _score)| {
            let file_path = file_map
                .get(&sym.file_id)
                .map(|f| f.path.clone())
                .unwrap_or_default();
            let component = component_map.get(&sym.file_id);
            let lines = sym.end_line - sym.start_line + 1;
            json!({
                "symbol": sym.qualified_name,
                "file": file_path,
                "kind": sym.kind,
                "lines": lines,
                "component": component,
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
        "strategy": select_strategy(total_hits, items.len()),
        "summary": {
            "total_items": items.len(),
            "direct_matches": items.len(),
            "fan_out_items": 0,
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
    fn strategy_zero_hits() {
        let s = select_strategy(0, 0);
        assert_eq!(s["action"], "narrow_query");
    }

    #[test]
    fn strategy_few_hits() {
        let s = select_strategy(2, 2);
        assert_eq!(s["action"], "read_all");
    }

    #[test]
    fn strategy_many_hits() {
        let s = select_strategy(15, 10);
        assert_eq!(s["action"], "narrow_query");
    }

    #[test]
    fn strategy_medium_hits() {
        let s = select_strategy(6, 6);
        assert_eq!(s["action"], "read_top_n");
        assert_eq!(s["n"], 3);
    }
}
