use std::collections::{HashMap, HashSet};

use uuid::Uuid;

use crate::db::{Db, SymbolRow};
use crate::error::Result;

pub const ANCHOR_KINDS: &[&str] = &[
    "function",
    "struct",
    "enum",
    "trait",
    "method",
    "type_alias",
    "const",
    "static",
];

pub fn anchor_count(eligible: usize) -> usize {
    (eligible / 8).clamp(3, 7)
}

fn tokenize_name(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in name.chars() {
        if ch == '_' || ch == '-' || ch == '/' {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current).to_lowercase());
            }
        } else if ch.is_uppercase() && !current.is_empty() {
            tokens.push(std::mem::take(&mut current).to_lowercase());
            current.push(ch);
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        tokens.push(current.to_lowercase());
    }
    tokens
}

fn name_alignment(symbol_name: &str, component_name: &str) -> f64 {
    let sym_tokens: HashSet<String> = tokenize_name(symbol_name).into_iter().collect();
    let comp_tokens: HashSet<String> = tokenize_name(component_name).into_iter().collect();
    if sym_tokens.is_empty() || comp_tokens.is_empty() {
        return 0.0;
    }
    let intersection = sym_tokens.intersection(&comp_tokens).count();
    let union = sym_tokens.union(&comp_tokens).count();
    intersection as f64 / union as f64
}

fn rank_normalize(values: &[f64]) -> Vec<f64> {
    let n = values.len();
    if n <= 1 {
        return vec![1.0; n];
    }
    let mut indexed: Vec<(usize, f64)> = values.iter().copied().enumerate().collect();
    indexed.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let mut ranks = vec![0.0; n];
    // Assign average rank to tied values
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j < n && (indexed[j].1 - indexed[i].1).abs() < f64::EPSILON {
            j += 1;
        }
        let avg_rank = (i + j - 1) as f64 / 2.0;
        for item in &indexed[i..j] {
            ranks[item.0] = avg_rank / (n - 1) as f64;
        }
        i = j;
    }
    ranks
}

struct ScoredSymbol {
    qualified_name: String,
    score: f64,
    rationale: String,
}

pub fn compute_semantic_anchors(
    db: &Db,
    gd: &crate::graph::GraphData,
    churn_map: &HashMap<String, u32>,
) -> Result<usize> {
    let components = db.all_components()?;
    if components.is_empty() {
        return Ok(0);
    }

    let files = db.all_files()?;
    let file_id_to_path: HashMap<i64, &str> = files.iter().map(|f| (f.id, &*f.path)).collect();
    let all_symbols = db.all_symbols_by_file()?;

    let mut component_file_ids: HashMap<String, Vec<i64>> = HashMap::new();
    let mut file_to_component: HashMap<i64, String> = HashMap::new();
    for c in &components {
        let fids = db.component_file_ids(&c.id)?;
        for &fid in &fids {
            file_to_component.insert(fid, c.id.clone());
        }
        component_file_ids.insert(c.id.clone(), fids);
    }

    let mut intra_in_degree: HashMap<i64, usize> = HashMap::new();
    for &(src_file_id, target_sym_id) in &gd.all_refs {
        if let Some(target_file_id) = gd.sym_to_file.get(&target_sym_id) {
            let src_comp = file_to_component.get(&src_file_id);
            let tgt_comp = file_to_component.get(target_file_id);
            if src_comp.is_some() && src_comp == tgt_comp {
                *intra_in_degree.entry(target_sym_id).or_default() += 1;
            }
        }
    }

    let mut all_anchors = Vec::new();
    let mut total = 0;

    for c in &components {
        let fids = match component_file_ids.get(&c.id) {
            Some(f) => f,
            None => continue,
        };

        let eligible: Vec<&SymbolRow> = fids
            .iter()
            .filter_map(|fid| all_symbols.get(fid))
            .flatten()
            .filter(|s| ANCHOR_KINDS.contains(&&*s.kind))
            .filter(|s| s.parent_symbol_id.is_none() || &*s.kind == "method")
            .collect();

        if eligible.is_empty() {
            continue;
        }

        let in_degrees: Vec<f64> = eligible
            .iter()
            .map(|s| intra_in_degree.get(&s.id).copied().unwrap_or(0) as f64)
            .collect();
        let pageranks: Vec<f64> = eligible.iter().map(|s| s.pagerank.unwrap_or(0.0)).collect();
        let stabilities: Vec<f64> = eligible
            .iter()
            .map(|s| {
                let path = file_id_to_path.get(&s.file_id).copied().unwrap_or("");
                let churn = churn_map.get(path).copied().unwrap_or(0) as f64;
                1.0 / (1.0 + churn)
            })
            .collect();
        let namings: Vec<f64> = eligible
            .iter()
            .map(|s| name_alignment(&s.short_name, &c.name))
            .collect();

        let in_degree_norm = rank_normalize(&in_degrees);
        let pagerank_norm = rank_normalize(&pageranks);
        let stability_norm = rank_normalize(&stabilities);
        let naming_norm = rank_normalize(&namings);

        let mut scored: Vec<ScoredSymbol> = eligible
            .iter()
            .enumerate()
            .map(|(i, sym)| {
                let score = 0.35 * in_degree_norm[i]
                    + 0.30 * pagerank_norm[i]
                    + 0.20 * stability_norm[i]
                    + 0.15 * naming_norm[i];
                let rationale = format!(
                    "in_degree={:.2} pagerank={:.2} stability={:.2} naming={:.2}",
                    in_degree_norm[i], pagerank_norm[i], stability_norm[i], naming_norm[i],
                );
                ScoredSymbol {
                    qualified_name: sym.qualified_name.to_string(),
                    score,
                    rationale,
                }
            })
            .collect();

        scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
        let n = anchor_count(eligible.len());
        scored.truncate(n);

        for s in &scored {
            all_anchors.push((
                Uuid::now_v7().to_string(),
                c.id.clone(),
                s.qualified_name.clone(),
                s.score,
                s.rationale.clone(),
            ));
        }
        total += scored.len();
    }

    db.replace_all_anchors(&all_anchors)?;
    Ok(total)
}

pub fn extract_stems(symbols: &[&SymbolRow]) -> usize {
    let mut stems: HashSet<String> = HashSet::new();
    for s in symbols {
        for token in tokenize_name(&s.short_name) {
            stems.insert(token);
        }
    }
    stems.len()
}

pub fn concept_density(symbols: &[&SymbolRow], total_loc: i64) -> f64 {
    if symbols.is_empty() || total_loc <= 0 {
        return 0.0;
    }
    let unique_kinds = symbols
        .iter()
        .map(|s| &*s.kind)
        .collect::<HashSet<_>>()
        .len();
    let stem_diversity = extract_stems(symbols);
    let raw = (unique_kinds as f64 * stem_diversity as f64) / total_loc as f64;
    (raw * 10000.0).round() / 10000.0
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn make_symbol(id: i64, short_name: &str, kind: &str) -> SymbolRow {
        SymbolRow {
            id,
            file_id: 1,
            qualified_name: Arc::from(short_name),
            short_name: Arc::from(short_name),
            kind: Arc::from(kind),
            signature: None,
            signature_hash: None,
            visibility: None,
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: None,
            pagerank: None,
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        }
    }

    #[test]
    fn test_extract_stems_diverse() {
        let syms = [
            make_symbol(1, "UserProfile", "struct"),
            make_symbol(2, "fetch_data", "function"),
            make_symbol(3, "render_chart", "function"),
        ];
        let refs: Vec<&SymbolRow> = syms.iter().collect();
        let count = extract_stems(&refs);
        // user, profile, fetch, data, render, chart = 6
        assert_eq!(count, 6);
    }

    #[test]
    fn test_extract_stems_repetitive() {
        let syms = [
            make_symbol(1, "handle_create", "function"),
            make_symbol(2, "handle_update", "function"),
            make_symbol(3, "handle_delete", "function"),
        ];
        let refs: Vec<&SymbolRow> = syms.iter().collect();
        let count = extract_stems(&refs);
        // handle, create, update, delete = 4
        assert_eq!(count, 4);
    }

    #[test]
    fn test_concept_density_formula() {
        // 2 kinds (struct, function) × 6 stems / 100 LOC = 0.12
        let syms = [
            make_symbol(1, "UserProfile", "struct"),
            make_symbol(2, "fetch_data", "function"),
            make_symbol(3, "render_chart", "function"),
        ];
        let refs: Vec<&SymbolRow> = syms.iter().collect();
        let d = concept_density(&refs, 100);
        assert!((d - 0.12).abs() < 0.001, "expected 0.12, got {d}");
    }

    #[test]
    fn test_concept_density_empty() {
        let refs: Vec<&SymbolRow> = vec![];
        assert_eq!(concept_density(&refs, 100), 0.0);
    }

    #[test]
    fn test_concept_density_zero_loc() {
        let syms = [make_symbol(1, "foo", "function")];
        let refs: Vec<&SymbolRow> = syms.iter().collect();
        assert_eq!(concept_density(&refs, 0), 0.0);
    }
}
