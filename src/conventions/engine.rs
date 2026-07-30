use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::context::FormalContext;

use super::SymbolAttrs;

#[derive(Debug, Clone, PartialEq)]
pub struct Convention {
    pub id: Arc<str>,
    pub antecedent: Arc<[String]>,
    pub consequent: Arc<[String]>,
    pub support: usize,
    pub confidence: f64,
    pub component_id: Option<String>,
}

impl From<crate::db::ConventionRow> for Convention {
    fn from(row: crate::db::ConventionRow) -> Self {
        Self {
            id: Arc::from(row.id),
            antecedent: Arc::from(
                row.antecedent
                    .split(", ")
                    .map(String::from)
                    .collect::<Vec<_>>(),
            ),
            consequent: Arc::from(
                row.consequent
                    .split(", ")
                    .map(String::from)
                    .collect::<Vec<_>>(),
            ),
            support: row.support as usize,
            confidence: row.confidence,
            component_id: row.component_id,
        }
    }
}

impl Convention {
    pub fn compute_id(
        antecedent: &[String],
        consequent: &[String],
        component_id: Option<&str>,
    ) -> Arc<str> {
        let mut ante = antecedent.to_vec();
        ante.sort();
        let mut cons = consequent.to_vec();
        cons.sort();
        let input = match component_id {
            Some(cid) => format!("{}\x02{}\0{}", cid, ante.join("\x1f"), cons.join("\x1f")),
            None => format!("{}\0{}", ante.join("\x1f"), cons.join("\x1f")),
        };
        Arc::from(&blake3::hash(input.as_bytes()).to_hex()[..16])
    }

    pub fn is_checkable(&self, toolchain_pairs: &[crate::parser::adapter::ToolchainPair]) -> bool {
        use super::attributes::{AttributeRole, classify_attribute};

        if !self
            .consequent
            .iter()
            .all(|c| classify_attribute(c) == AttributeRole::Obligation)
        {
            return false;
        }

        for pair in toolchain_pairs {
            if self.consequent.len() == 1
                && self.antecedent.iter().any(|a| a == pair.antecedent)
                && self.consequent[0] == pair.consequent
            {
                return false;
            }
        }

        true
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConventionMatch {
    pub symbol: String,
    pub file: String,
    pub convention_id: Arc<str>,
    pub antecedent: Arc<[String]>,
    pub consequent: Arc<[String]>,
    pub lifecycle_state: String,
    pub support: usize,
    pub confidence: f64,
}

#[derive(Debug, Clone)]
pub struct Deviation {
    pub symbol: String,
    pub file: String,
    pub pattern_antecedent: Arc<[String]>,
    pub pattern_consequent: Arc<[String]>,
    pub missing: Vec<String>,
    pub support: usize,
    pub confidence: f64,
    pub total_matching: usize,
    pub conforming: usize,
    pub exemplars: Vec<String>,
    pub strength: f64,
    pub informational: bool,
}

#[derive(Debug, Clone)]
pub struct ObservedPattern {
    pub antecedent: Arc<[String]>,
    pub consequent: Arc<[String]>,
    pub support: usize,
    pub confidence: f64,
    pub total_matching: usize,
    pub exemplars: Vec<String>,
}

const MIN_SUPPORT: usize = 3;
pub const MIN_CONFIDENCE: f64 = 0.9;
const MAX_COMPONENT_SUPPORT: usize = 20;
const MAX_DEVIATIONS: usize = 5;
const MAX_ORIENT_PATTERNS: usize = 5;

pub fn component_min_support(component_size: usize) -> usize {
    ((component_size as f64 * 0.4).ceil() as usize).clamp(2, MAX_COMPONENT_SUPPORT)
}

pub struct FcaEngine {
    context: Option<FormalContext>,
    conventions: Vec<Convention>,
    symbol_attrs: Vec<SymbolAttrs>,
    last_matrix_hash: Option<blake3::Hash>,
}

impl Default for FcaEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl FcaEngine {
    pub fn new() -> Self {
        Self {
            context: None,
            conventions: Vec::new(),
            symbol_attrs: Vec::new(),
            last_matrix_hash: None,
        }
    }

    pub fn rebuild(&mut self, symbols: &[SymbolAttrs]) {
        self.rebuild_with_params(symbols, MIN_SUPPORT, MIN_CONFIDENCE, None)
    }

    pub fn rebuild_with_params(
        &mut self,
        symbols: &[SymbolAttrs],
        min_support: usize,
        min_confidence: f64,
        component_id: Option<&str>,
    ) {
        let hash = Self::hash_matrix(symbols, min_support, min_confidence);
        if self.last_matrix_hash == Some(hash) {
            return;
        }

        let (ctx, _attr_names) = Self::build_context(symbols);
        let impls = ctx.approximate_implications(min_support, min_confidence);

        self.conventions = impls
            .into_iter()
            .map(|imp| {
                let id = Convention::compute_id(&imp.antecedent, &imp.consequent, component_id);
                Convention {
                    id,
                    antecedent: Arc::from(imp.antecedent),
                    consequent: Arc::from(imp.consequent),
                    support: imp.support,
                    confidence: imp.confidence,
                    component_id: component_id.map(|s| s.to_string()),
                }
            })
            .collect();

        self.context = Some(ctx);
        self.symbol_attrs = symbols.to_vec();
        self.last_matrix_hash = Some(hash);
    }

    pub fn update_incremental(&mut self, added: &[SymbolAttrs], removed: &[String]) {
        self.symbol_attrs.retain(|s| !removed.contains(&s.name));
        for sa in added {
            self.symbol_attrs.retain(|s| s.name != sa.name);
            self.symbol_attrs.push(sa.clone());
        }
        self.rebuild(&self.symbol_attrs.clone())
    }

    pub fn set_conventions(&mut self, conventions: Vec<Convention>) {
        self.conventions = conventions;
    }

    pub fn conventions(&self) -> &[Convention] {
        &self.conventions
    }

    pub fn check_inverse(
        &self,
        changed_symbols: &[SymbolAttrs],
        deprecated_ids: &HashSet<String>,
        forbidden_ids: &HashSet<String>,
    ) -> Vec<ConventionMatch> {
        let mut matches = Vec::new();

        for conv in &self.conventions {
            let lifecycle = if deprecated_ids.contains(&*conv.id) {
                "deprecated"
            } else if forbidden_ids.contains(&*conv.id) {
                "forbidden"
            } else {
                continue;
            };

            for sym in changed_symbols {
                if let Some(conv_comp) = &conv.component_id
                    && sym.component_id.as_ref() != Some(conv_comp)
                {
                    continue;
                }

                let attrs: HashSet<&str> = sym.attributes.iter().map(|a| a.as_str()).collect();
                let has_antecedent = conv.antecedent.iter().all(|a| attrs.contains(a.as_str()));
                let has_consequent = conv.consequent.iter().all(|c| attrs.contains(c.as_str()));

                if has_antecedent && has_consequent {
                    matches.push(ConventionMatch {
                        symbol: sym.name.clone(),
                        file: sym.file.clone(),
                        convention_id: Arc::clone(&conv.id),
                        antecedent: Arc::clone(&conv.antecedent),
                        consequent: Arc::clone(&conv.consequent),
                        lifecycle_state: lifecycle.to_string(),
                        support: conv.support,
                        confidence: conv.confidence,
                    });
                }
            }
        }

        matches
    }

    const MAX_ATTRS: usize = 100;

    fn hash_matrix(
        symbols: &[SymbolAttrs],
        min_support: usize,
        min_confidence: f64,
    ) -> blake3::Hash {
        let mut sorted: Vec<(&str, Vec<&str>)> = symbols
            .iter()
            .map(|s| {
                let mut attrs: Vec<&str> = s.attributes.iter().map(|a| a.as_str()).collect();
                attrs.sort_unstable();
                (s.name.as_str(), attrs)
            })
            .collect();
        sorted.sort_unstable_by_key(|(name, _)| *name);

        let mut hasher = blake3::Hasher::new();
        hasher.update(&min_support.to_le_bytes());
        hasher.update(&min_confidence.to_le_bytes());
        for (name, attrs) in &sorted {
            hasher.update(name.as_bytes());
            hasher.update(b"\0");
            for attr in attrs {
                hasher.update(attr.as_bytes());
                hasher.update(b"\x1f");
            }
            hasher.update(b"\n");
        }
        hasher.finalize()
    }

    fn build_context(symbols: &[SymbolAttrs]) -> (FormalContext, Vec<String>) {
        let mut attr_freq: HashMap<String, usize> = HashMap::new();
        for sym in symbols {
            for attr in &sym.attributes {
                *attr_freq.entry(attr.clone()).or_default() += 1;
            }
        }

        let allowed: HashSet<&str> = if attr_freq.len() > Self::MAX_ATTRS {
            let mut by_freq: Vec<(&str, usize)> =
                attr_freq.iter().map(|(k, &v)| (k.as_str(), v)).collect();
            by_freq.sort_by_key(|x| std::cmp::Reverse(x.1));
            by_freq.truncate(Self::MAX_ATTRS);
            by_freq.into_iter().map(|(k, _)| k).collect()
        } else {
            attr_freq.keys().map(|k| k.as_str()).collect()
        };

        let mut attr_map: HashMap<String, usize> = HashMap::new();
        let mut relations: Vec<(usize, usize)> = Vec::new();

        for (obj_idx, sym) in symbols.iter().enumerate() {
            for attr in &sym.attributes {
                if !allowed.contains(attr.as_str()) {
                    continue;
                }
                let attr_idx = {
                    let len = attr_map.len();
                    *attr_map.entry(attr.clone()).or_insert(len)
                };
                relations.push((obj_idx, attr_idx));
            }
        }

        let mut attr_names_sorted: Vec<(String, usize)> = attr_map.into_iter().collect();
        attr_names_sorted.sort_by_key(|(_, idx)| *idx);
        let attr_names: Vec<String> = attr_names_sorted.into_iter().map(|(n, _)| n).collect();

        let object_names: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();

        let ctx = FormalContext::new(object_names, attr_names.clone(), &relations);
        (ctx, attr_names)
    }
}

pub fn deduplicate_component_conventions(
    component_convs: Vec<Convention>,
    global_convs: &[Convention],
) -> Vec<Convention> {
    component_convs
        .into_iter()
        .filter(|cc| {
            !global_convs.iter().any(|gc| {
                gc.antecedent == cc.antecedent
                    && gc.consequent == cc.consequent
                    && gc.confidence >= cc.confidence
            })
        })
        .collect()
}

pub fn detect_deviations(
    changed_sym_attrs: &[SymbolAttrs],
    all_sym_attrs_by_component: &HashMap<String, Vec<SymbolAttrs>>,
    orphan_sym_attrs: &[SymbolAttrs],
    toolchain_pairs: &[crate::parser::adapter::ToolchainPair],
    sketch_components: &HashSet<String>,
) -> Vec<Deviation> {
    use super::attributes::{AttributeRole, classify_attribute};

    let mut all_deviations = Vec::new();

    let check_group = |symbols: &[SymbolAttrs],
                       component_id: Option<&str>,
                       informational: bool,
                       out: &mut Vec<Deviation>| {
        if symbols.len() < 2 {
            return;
        }
        let min_support = match component_id {
            Some(_) => component_min_support(symbols.len()),
            None => MIN_SUPPORT,
        };
        let mut engine = FcaEngine::new();
        engine.rebuild_with_params(symbols, min_support, MIN_CONFIDENCE, component_id);

        let ctx = match &engine.context {
            Some(c) => c,
            None => return,
        };

        for conv in engine.conventions() {
            if !conv
                .antecedent
                .iter()
                .all(|a| classify_attribute(a) == AttributeRole::Identity)
            {
                continue;
            }
            if !conv
                .consequent
                .iter()
                .all(|c| classify_attribute(c) == AttributeRole::Obligation)
            {
                continue;
            }
            if !conv.is_checkable(toolchain_pairs) {
                continue;
            }

            let ante_strs: Vec<&str> = conv.antecedent.iter().map(|s| s.as_str()).collect();
            let cons_strs: Vec<&str> = conv.consequent.iter().map(|s| s.as_str()).collect();
            let total_matching = ctx.count_with_attrs(&ante_strs);

            for sym in changed_sym_attrs {
                if sym.component_id.as_deref() != component_id {
                    continue;
                }
                let attrs: HashSet<&str> = sym.attributes.iter().map(|a| a.as_str()).collect();
                if !conv.antecedent.iter().all(|a| attrs.contains(a.as_str())) {
                    continue;
                }
                let missing: Vec<String> = conv
                    .consequent
                    .iter()
                    .filter(|c| !attrs.contains(c.as_str()))
                    .cloned()
                    .collect();
                if missing.is_empty() {
                    continue;
                }

                let exemplars: Vec<String> = ctx
                    .exemplars_for(&ante_strs, &cons_strs, 3)
                    .into_iter()
                    .filter(|e| *e != sym.name)
                    .take(3)
                    .map(|s| s.to_string())
                    .collect();

                out.push(Deviation {
                    symbol: sym.name.clone(),
                    file: sym.file.clone(),
                    pattern_antecedent: Arc::clone(&conv.antecedent),
                    pattern_consequent: Arc::clone(&conv.consequent),
                    missing,
                    support: conv.support,
                    confidence: conv.confidence,
                    total_matching,
                    conforming: conv.support,
                    exemplars,
                    strength: conv.support as f64 * conv.confidence,
                    informational,
                });
            }
        }
    };

    for (comp_id, symbols) in all_sym_attrs_by_component {
        let informational = sketch_components.contains(comp_id);
        check_group(symbols, Some(comp_id), informational, &mut all_deviations);
    }

    if !orphan_sym_attrs.is_empty() {
        let all_symbols: Vec<SymbolAttrs> = all_sym_attrs_by_component
            .values()
            .flatten()
            .chain(orphan_sym_attrs.iter())
            .cloned()
            .collect();
        check_group(&all_symbols, None, false, &mut all_deviations);
    }

    let strength_cmp = |a: &Deviation, b: &Deviation| {
        b.strength
            .partial_cmp(&a.strength)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.support.cmp(&a.support))
    };

    let (mut actionable, mut informational): (Vec<_>, Vec<_>) =
        all_deviations.into_iter().partition(|d| !d.informational);

    actionable.sort_by(&strength_cmp);
    actionable.truncate(MAX_DEVIATIONS);

    let remaining = MAX_DEVIATIONS.saturating_sub(actionable.len());
    if remaining > 0 {
        informational.sort_by(&strength_cmp);
        informational.truncate(remaining);
        actionable.extend(informational);
    }

    actionable
}

pub fn describe_patterns(
    sym_attrs: &[SymbolAttrs],
    component_id: Option<&str>,
    toolchain_pairs: &[crate::parser::adapter::ToolchainPair],
) -> Vec<ObservedPattern> {
    use super::attributes::{AttributeRole, classify_attribute};

    if sym_attrs.len() < 2 {
        return Vec::new();
    }
    let min_support = match component_id {
        Some(_) => component_min_support(sym_attrs.len()),
        None => MIN_SUPPORT,
    };
    let mut engine = FcaEngine::new();
    engine.rebuild_with_params(sym_attrs, min_support, MIN_CONFIDENCE, component_id);
    let ctx = match &engine.context {
        Some(c) => c,
        None => return Vec::new(),
    };

    let mut patterns: Vec<ObservedPattern> = engine
        .conventions()
        .iter()
        .filter(|c| {
            c.antecedent
                .iter()
                .all(|a| classify_attribute(a) == AttributeRole::Identity)
                && c.consequent
                    .iter()
                    .all(|attr| classify_attribute(attr) == AttributeRole::Obligation)
                && c.is_checkable(toolchain_pairs)
        })
        .map(|c| {
            let ante: Vec<&str> = c.antecedent.iter().map(|s| s.as_str()).collect();
            let cons: Vec<&str> = c.consequent.iter().map(|s| s.as_str()).collect();
            let total_matching = ctx.count_with_attrs(&ante);
            let exemplars: Vec<String> = ctx
                .exemplars_for(&ante, &cons, 2)
                .into_iter()
                .map(|s| s.to_string())
                .collect();
            ObservedPattern {
                antecedent: Arc::clone(&c.antecedent),
                consequent: Arc::clone(&c.consequent),
                support: c.support,
                confidence: c.confidence,
                total_matching,
                exemplars,
            }
        })
        .collect();

    let strength = |p: &ObservedPattern| p.support as f64 * p.confidence;
    patterns.sort_by(|a, b| {
        strength(b)
            .partial_cmp(&strength(a))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.support.cmp(&a.support))
    });
    patterns.truncate(MAX_ORIENT_PATTERNS);
    patterns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_hash_same_inputs_same_output() {
        let id1 = Convention::compute_id(&["kind:function".into()], &["has_sig".into()], None);
        let id2 = Convention::compute_id(&["kind:function".into()], &["has_sig".into()], None);
        assert_eq!(id1, id2);
    }

    #[test]
    fn stable_hash_order_independent() {
        let id1 =
            Convention::compute_id(&["a".into(), "b".into()], &["c".into(), "d".into()], None);
        let id2 =
            Convention::compute_id(&["b".into(), "a".into()], &["d".into(), "c".into()], None);
        assert_eq!(id1, id2);
    }

    #[test]
    fn stable_hash_different_for_different_implications() {
        let id1 = Convention::compute_id(&["kind:function".into()], &["has_sig".into()], None);
        let id2 =
            Convention::compute_id(&["kind:struct".into()], &["naming:CamelCase".into()], None);
        assert_ne!(id1, id2);
    }

    #[test]
    fn rebuild_produces_conventions() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        engine.rebuild(&symbols);
        let conventions = engine.conventions();
        assert!(!conventions.is_empty());
        let found = conventions.iter().any(|c| {
            c.antecedent.to_vec() == vec!["kind:function"]
                && c.consequent.to_vec() == vec!["has_sig"]
        });
        assert!(found, "expected kind:function → has_sig");
    }

    #[test]
    fn rebuild_produces_stable_ids_across_calls() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        engine.rebuild(&symbols);
        let ids1: Vec<Arc<str>> = engine
            .conventions()
            .iter()
            .map(|c| Arc::clone(&c.id))
            .collect();
        engine.rebuild(&symbols);
        let ids2: Vec<Arc<str>> = engine
            .conventions()
            .iter()
            .map(|c| Arc::clone(&c.id))
            .collect();
        assert_eq!(ids1, ids2);
    }

    #[test]
    fn rebuild_skips_when_input_unchanged() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        engine.rebuild(&symbols);
        let first = engine.conventions().to_vec();
        assert!(engine.last_matrix_hash.is_some());
        let hash_after_first = engine.last_matrix_hash;

        engine.rebuild(&symbols);
        assert_eq!(first, engine.conventions());
        assert_eq!(engine.last_matrix_hash, hash_after_first);
    }

    #[test]
    fn rebuild_recomputes_when_input_changes() {
        let mut symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        engine.rebuild(&symbols);
        let hash_before = engine.last_matrix_hash;

        symbols.push(SymbolAttrs {
            name: "new_struct".into(),
            file: "new.rs".into(),
            attributes: vec!["kind:struct".into(), "naming:CamelCase".into()],
            component_id: None,
        });
        engine.rebuild(&symbols);
        assert_ne!(engine.last_matrix_hash, hash_before);
    }

    #[test]
    fn incremental_add_changes_conventions() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        engine.rebuild(&symbols);
        let before = engine.conventions().to_vec();

        // Add another function WITHOUT has_sig → lowers confidence
        let extra = SymbolAttrs {
            name: "fn_extra".into(),
            file: "src/test.rs".into(),
            attributes: vec!["kind:function".into()],
            component_id: None,
        };
        engine.update_incremental(&[extra], &[]);

        let before_conf = before
            .iter()
            .find(|c| {
                c.antecedent.to_vec() == vec!["kind:function"]
                    && c.consequent.to_vec() == vec!["has_sig"]
            })
            .map(|c| c.confidence);
        let after_conf = engine
            .conventions()
            .iter()
            .find(|c| {
                c.antecedent.to_vec() == vec!["kind:function"]
                    && c.consequent.to_vec() == vec!["has_sig"]
            })
            .map(|c| c.confidence);

        match (before_conf, after_conf) {
            (Some(b), Some(a)) => assert!(a < b, "confidence should decrease"),
            (Some(_), None) => {} // dropped below threshold — also valid
            _ => panic!("expected convention present before add"),
        }
    }

    #[test]
    fn incremental_remove_changes_conventions() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        engine.rebuild(&symbols);

        // Remove the one function that lacks has_sig → confidence goes to 1.0
        engine.update_incremental(&[], &["fn_9".into()]);
        let found = engine.conventions().iter().find(|c| {
            c.antecedent.to_vec() == vec!["kind:function"]
                && c.consequent.to_vec() == vec!["has_sig"]
        });
        // At 1.0 confidence it should now be included
        assert!(found.is_some());
        assert!((found.unwrap().confidence - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn incremental_add_is_idempotent() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        engine.rebuild(&symbols);
        let first = engine.conventions().to_vec();

        let extra = SymbolAttrs {
            name: "fn_0".into(),
            file: "src/test.rs".into(),
            attributes: vec!["kind:function".into(), "has_sig".into()],
            component_id: None,
        };
        engine.update_incremental(std::slice::from_ref(&extra), &[]);
        let second = engine.conventions().to_vec();
        engine.update_incremental(&[extra], &[]);
        let third = engine.conventions();

        let count_fn = |convs: &[Convention]| {
            convs
                .iter()
                .find(|c| {
                    c.antecedent.to_vec() == vec!["kind:function"]
                        && c.consequent.to_vec() == vec!["has_sig"]
                })
                .map(|c| (c.support, c.confidence))
        };
        assert_eq!(
            count_fn(&second),
            count_fn(third),
            "replay should not change stats"
        );
        assert_eq!(
            count_fn(&first),
            count_fn(&second),
            "replacing with same data should be stable"
        );
    }

    #[test]
    fn realistic_codebase_produces_expected_conventions() {
        let symbols = make_realistic_symbols();
        let mut engine = FcaEngine::new();
        engine.rebuild(&symbols);
        let conventions = engine.conventions();

        let has = |ante: &str, cons: &str| -> bool {
            conventions.iter().any(|c| {
                c.antecedent.contains(&ante.to_string()) && c.consequent.contains(&cons.to_string())
            })
        };

        // Functions with signatures: most have has_sig + naming:snake_case
        // Spike found: has_sig ↔ naming:snake_case as a strong pattern
        assert!(
            has("has_sig", "naming:snake_case") || has("naming:snake_case", "has_sig"),
            "expected has_sig ↔ naming:snake_case convention"
        );

        // Methods with self refs: takes_self_ref → is_method is exact (1.0),
        // so it's correctly excluded from approximate implications.
        // Instead check that kind:method → takes_self_ref appears (19/20 = 0.95)
        assert!(
            has("kind:method", "takes_self_ref"),
            "expected kind:method → takes_self_ref (approximate at 0.95)"
        );

        // All conventions should have confidence >= 0.9
        for c in conventions {
            assert!(
                c.confidence >= 0.9,
                "convention {:?} → {:?} has confidence {} < 0.9",
                c.antecedent,
                c.consequent,
                c.confidence
            );
        }
    }

    fn make_realistic_symbols() -> Vec<SymbolAttrs> {
        let mut symbols = Vec::new();

        // 40 functions: all snake_case, 38 have signatures (95%)
        for i in 0..40 {
            let mut attrs = vec![
                "kind:function".into(),
                "naming:snake_case".into(),
                "vis:pub".into(),
            ];
            if i < 38 {
                attrs.push("has_sig".into());
            }
            if i < 30 {
                attrs.push("returns_result".into());
            }
            if i % 5 == 0 {
                attrs.push("has_doc".into());
            }
            symbols.push(SymbolAttrs {
                name: format!("fn_{i}"),
                file: format!("src/funcs/{i}.rs"),
                attributes: attrs,
                component_id: None,
            });
        }

        // 20 methods: all snake_case, all have sig, all is_method
        for i in 0..20 {
            let mut attrs = vec![
                "kind:method".into(),
                "naming:snake_case".into(),
                "has_sig".into(),
                "is_method".into(),
            ];
            if i < 19 {
                attrs.push("takes_self_ref".into());
            }
            if i < 15 {
                attrs.push("complexity:low".into());
            }
            symbols.push(SymbolAttrs {
                name: format!("method_{i}"),
                file: format!("src/methods/{i}.rs"),
                attributes: attrs,
                component_id: None,
            });
        }

        // 15 structs: CamelCase, pub
        for i in 0..15 {
            let mut attrs = vec![
                "kind:struct".into(),
                "naming:CamelCase".into(),
                "vis:pub".into(),
            ];
            if i < 12 {
                attrs.push("has_doc".into());
            }
            symbols.push(SymbolAttrs {
                name: format!("Struct{i}"),
                file: format!("src/types/{i}.rs"),
                attributes: attrs,
                component_id: None,
            });
        }

        // 8 enums: CamelCase
        for i in 0..8 {
            symbols.push(SymbolAttrs {
                name: format!("Enum{i}"),
                file: format!("src/enums/{i}.rs"),
                attributes: vec![
                    "kind:enum".into(),
                    "naming:CamelCase".into(),
                    "vis:pub".into(),
                ],
                component_id: None,
            });
        }

        symbols
    }

    fn make_test_symbols() -> Vec<SymbolAttrs> {
        let mut symbols = Vec::new();
        // 10 functions: 9 have has_sig, 1 doesn't
        for i in 0..10 {
            let mut attrs = vec!["kind:function".into()];
            if i < 9 {
                attrs.push("has_sig".into());
            }
            symbols.push(SymbolAttrs {
                name: format!("fn_{i}"),
                file: format!("src/test_{i}.rs"),
                attributes: attrs,
                component_id: None,
            });
        }
        // 5 structs with naming:CamelCase
        for i in 0..5 {
            symbols.push(SymbolAttrs {
                name: format!("Struct{i}"),
                file: format!("src/structs/{i}.rs"),
                attributes: vec!["kind:struct".into(), "naming:CamelCase".into()],
                component_id: None,
            });
        }
        symbols
    }

    #[test]
    fn rebuild_with_params_uses_custom_threshold() {
        let symbols = make_test_symbols();
        let mut engine_high = FcaEngine::new();
        engine_high.rebuild_with_params(&symbols, 10, 0.9, None);

        let mut engine_low = FcaEngine::new();
        engine_low.rebuild_with_params(&symbols, 2, 0.9, None);

        assert!(
            engine_high.conventions().len() <= engine_low.conventions().len(),
            "higher min_support should produce fewer or equal conventions"
        );
    }

    #[test]
    fn rebuild_with_params_tags_component_id() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        engine.rebuild_with_params(&symbols, 3, 0.9, Some("comp-a"));
        let convs = engine.conventions();
        assert!(!convs.is_empty());
        for c in convs {
            assert_eq!(c.component_id.as_deref(), Some("comp-a"));
        }
    }

    #[test]
    fn rebuild_without_component_produces_global_conventions() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        engine.rebuild(&symbols);
        let convs = engine.conventions();
        assert!(!convs.is_empty());
        for c in convs {
            assert!(c.component_id.is_none());
        }
    }

    #[test]
    fn adaptive_threshold_discovers_component_local_pattern() {
        // 12 symbols in a component: 11 have has_doc, 1 doesn't.
        // Adaptive min_support = max(2, ceil(12 * 0.4)) = 5
        // Support = 11 (≥ 5), confidence = 11/12 ≈ 0.917 (≥ 0.9)
        // This convention would also be found globally at MIN_SUPPORT=3, but the test
        // verifies that the adaptive threshold + component tagging works end to end.
        let mut symbols = Vec::new();
        for i in 0..12 {
            let mut attrs = vec!["kind:function".into()];
            if i < 11 {
                attrs.push("has_doc".into());
            }
            symbols.push(SymbolAttrs {
                name: format!("comp_fn_{i}"),
                file: format!("src/comp/{i}.rs"),
                attributes: attrs,
                component_id: Some("comp-x".into()),
            });
        }
        let min_support = component_min_support(symbols.len());
        assert_eq!(min_support, 5);

        let mut engine = FcaEngine::new();
        engine.rebuild_with_params(&symbols, min_support, MIN_CONFIDENCE, Some("comp-x"));
        let has_fn_doc = engine.conventions().iter().any(|c| {
            c.antecedent.contains(&"kind:function".to_string())
                && c.consequent.contains(&"has_doc".to_string())
        });
        assert!(
            has_fn_doc,
            "component FCA should discover kind:function → has_doc"
        );
        assert!(
            engine
                .conventions()
                .iter()
                .all(|c| c.component_id.as_deref() == Some("comp-x"))
        );
    }

    #[test]
    fn adaptive_threshold_scales_with_size() {
        assert_eq!(component_min_support(3), 2);
        assert_eq!(component_min_support(5), 2);
        assert_eq!(component_min_support(10), 4);
        assert_eq!(component_min_support(50), 20);
        assert_eq!(component_min_support(100), 20); // caps at 20
        assert_eq!(component_min_support(200), 20); // caps at 20
    }

    #[test]
    fn dedup_removes_subsumed_component_conventions() {
        let global = vec![Convention {
            id: Convention::compute_id(&["kind:function".into()], &["has_sig".into()], None),
            antecedent: vec!["kind:function".into()].into(),
            consequent: vec!["has_sig".into()].into(),
            support: 10,
            confidence: 0.95,
            component_id: None,
        }];
        let comp = vec![Convention {
            id: Convention::compute_id(&["kind:function".into()], &["has_sig".into()], Some("c1")),
            antecedent: vec!["kind:function".into()].into(),
            consequent: vec!["has_sig".into()].into(),
            support: 3,
            confidence: 0.9,
            component_id: Some("c1".into()),
        }];
        let result = deduplicate_component_conventions(comp, &global);
        assert!(
            result.is_empty(),
            "component convention subsumed by global should be dropped"
        );
    }

    #[test]
    fn dedup_keeps_non_subsumed_component_conventions() {
        let global = vec![Convention {
            id: Convention::compute_id(&["kind:function".into()], &["has_sig".into()], None),
            antecedent: vec!["kind:function".into()].into(),
            consequent: vec!["has_sig".into()].into(),
            support: 10,
            confidence: 0.95,
            component_id: None,
        }];
        let comp = vec![Convention {
            id: Convention::compute_id(&["kind:struct".into()], &["has_doc".into()], Some("c1")),
            antecedent: vec!["kind:struct".into()].into(),
            consequent: vec!["has_doc".into()].into(),
            support: 5,
            confidence: 0.92,
            component_id: Some("c1".into()),
        }];
        let result = deduplicate_component_conventions(comp, &global);
        assert_eq!(
            result.len(),
            1,
            "different implication should survive dedup"
        );
    }

    #[test]
    fn dedup_keeps_higher_confidence_component_convention() {
        let global = vec![Convention {
            id: Convention::compute_id(&["kind:function".into()], &["has_sig".into()], None),
            antecedent: vec!["kind:function".into()].into(),
            consequent: vec!["has_sig".into()].into(),
            support: 10,
            confidence: 0.85,
            component_id: None,
        }];
        let comp = vec![Convention {
            id: Convention::compute_id(&["kind:function".into()], &["has_sig".into()], Some("c1")),
            antecedent: vec!["kind:function".into()].into(),
            consequent: vec!["has_sig".into()].into(),
            support: 5,
            confidence: 0.95,
            component_id: Some("c1".into()),
        }];
        let result = deduplicate_component_conventions(comp, &global);
        assert_eq!(
            result.len(),
            1,
            "component convention with higher confidence than global should survive"
        );
    }

    #[test]
    fn compute_id_differs_by_component_scope() {
        let global_id =
            Convention::compute_id(&["kind:function".into()], &["has_sig".into()], None);
        let comp_id = Convention::compute_id(
            &["kind:function".into()],
            &["has_sig".into()],
            Some("comp-1"),
        );
        assert_ne!(
            global_id, comp_id,
            "same implication in different scopes should have different IDs"
        );
    }

    #[test]
    fn check_inverse_detects_deprecated_pattern_match() {
        let mut engine = FcaEngine::new();
        engine.set_conventions(vec![Convention {
            id: "dep-1".into(),
            antecedent: vec!["kind:function".into()].into(),
            consequent: vec!["has_old_prefix".into()].into(),
            support: 5,
            confidence: 0.95,
            component_id: None,
        }]);

        let sym = SymbolAttrs {
            name: "old_process".into(),
            file: "src/lib.rs".into(),
            attributes: vec!["kind:function".into(), "has_old_prefix".into()],
            component_id: None,
        };

        let deprecated: HashSet<String> = ["dep-1".into()].into();
        let forbidden: HashSet<String> = HashSet::new();
        let matches = engine.check_inverse(&[sym], &deprecated, &forbidden);

        assert_eq!(matches.len(), 1);
        assert_eq!(&*matches[0].convention_id, "dep-1");
        assert_eq!(matches[0].lifecycle_state, "deprecated");
    }

    #[test]
    fn check_inverse_detects_forbidden_pattern_match() {
        let mut engine = FcaEngine::new();
        engine.set_conventions(vec![Convention {
            id: "forb-1".into(),
            antecedent: vec!["kind:function".into()].into(),
            consequent: vec!["uses_unsafe".into()].into(),
            support: 5,
            confidence: 0.95,
            component_id: None,
        }]);

        let sym = SymbolAttrs {
            name: "risky".into(),
            file: "src/lib.rs".into(),
            attributes: vec!["kind:function".into(), "uses_unsafe".into()],
            component_id: None,
        };

        let deprecated: HashSet<String> = HashSet::new();
        let forbidden: HashSet<String> = ["forb-1".into()].into();
        let matches = engine.check_inverse(&[sym], &deprecated, &forbidden);

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].lifecycle_state, "forbidden");
    }

    #[test]
    fn check_inverse_ignores_partial_match() {
        let mut engine = FcaEngine::new();
        engine.set_conventions(vec![Convention {
            id: "dep-1".into(),
            antecedent: vec!["kind:function".into()].into(),
            consequent: vec!["has_old_prefix".into()].into(),
            support: 5,
            confidence: 0.95,
            component_id: None,
        }]);

        // Has antecedent but NOT consequent — this is a violation, not an inverse match
        let sym = SymbolAttrs {
            name: "process".into(),
            file: "src/lib.rs".into(),
            attributes: vec!["kind:function".into()],
            component_id: None,
        };

        let deprecated: HashSet<String> = ["dep-1".into()].into();
        let matches = engine.check_inverse(&[sym], &deprecated, &HashSet::new());
        assert!(matches.is_empty());
    }

    #[test]
    fn check_inverse_ignores_non_deprecated_conventions() {
        let mut engine = FcaEngine::new();
        engine.set_conventions(vec![Convention {
            id: "conv-1".into(),
            antecedent: vec!["kind:function".into()].into(),
            consequent: vec!["has_return_type".into()].into(),
            support: 5,
            confidence: 0.95,
            component_id: None,
        }]);

        let sym = SymbolAttrs {
            name: "process".into(),
            file: "src/lib.rs".into(),
            attributes: vec!["kind:function".into(), "has_return_type".into()],
            component_id: None,
        };

        // conv-1 is not in deprecated or forbidden sets
        let matches = engine.check_inverse(&[sym], &HashSet::new(), &HashSet::new());
        assert!(matches.is_empty());
    }

    #[test]
    fn check_inverse_respects_component_scope() {
        let mut engine = FcaEngine::new();
        engine.set_conventions(vec![Convention {
            id: "dep-comp".into(),
            antecedent: vec!["kind:function".into()].into(),
            consequent: vec!["has_old_prefix".into()].into(),
            support: 5,
            confidence: 0.95,
            component_id: Some("comp-a".into()),
        }]);

        let sym_in_comp = SymbolAttrs {
            name: "old_process".into(),
            file: "src/a.rs".into(),
            attributes: vec!["kind:function".into(), "has_old_prefix".into()],
            component_id: Some("comp-a".into()),
        };
        let sym_other_comp = SymbolAttrs {
            name: "old_process".into(),
            file: "src/b.rs".into(),
            attributes: vec!["kind:function".into(), "has_old_prefix".into()],
            component_id: Some("comp-b".into()),
        };

        let deprecated: HashSet<String> = ["dep-comp".into()].into();
        let matches =
            engine.check_inverse(&[sym_in_comp, sym_other_comp], &deprecated, &HashSet::new());

        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].symbol, "old_process");
        assert_eq!(matches[0].file, "src/a.rs");
    }

    #[test]
    fn is_checkable_excludes_tautological_convention() {
        let conv = Convention {
            id: Arc::from("test-tautology"),
            antecedent: Arc::from(vec!["in:src/db".into()]),
            consequent: Arc::from(vec!["kind:function".into()]),
            support: 10,
            confidence: 0.9,
            component_id: None,
        };
        assert!(!conv.is_checkable(&[]));
    }

    #[test]
    fn is_checkable_excludes_backwards_convention() {
        let conv = Convention {
            id: Arc::from("test-backwards"),
            antecedent: Arc::from(vec!["has_doc".into()]),
            consequent: Arc::from(vec!["vis:pub".into()]),
            support: 10,
            confidence: 0.9,
            component_id: None,
        };
        assert!(!conv.is_checkable(&[]));
    }

    #[test]
    fn is_checkable_allows_vis_pub_implies_has_doc() {
        let conv = Convention {
            id: Arc::from("test-valid"),
            antecedent: Arc::from(vec!["vis:pub".into()]),
            consequent: Arc::from(vec!["has_doc".into()]),
            support: 10,
            confidence: 0.9,
            component_id: None,
        };
        assert!(conv.is_checkable(&[]));
    }

    #[test]
    fn is_checkable_excludes_toolchain_enforced() {
        use crate::parser::adapter::ToolchainPair;
        let conv = Convention {
            id: Arc::from("test-toolchain"),
            antecedent: Arc::from(vec!["kind:function".into()]),
            consequent: Arc::from(vec!["naming:snake_case".into()]),
            support: 50,
            confidence: 1.0,
            component_id: None,
        };
        let pairs = &[ToolchainPair {
            antecedent: "kind:function",
            consequent: "naming:snake_case",
        }];
        assert!(!conv.is_checkable(pairs));
    }

    #[test]
    fn detect_deviations_excludes_obligation_antecedent() {
        let symbols: Vec<SymbolAttrs> = (0..6)
            .map(|i| SymbolAttrs {
                name: format!("fn_{i}"),
                file: "src/test.rs".into(),
                attributes: vec![
                    "kind:function".into(),
                    "has_doc".into(),
                    "returns_result".into(),
                ],
                component_id: Some("comp".into()),
            })
            .chain(std::iter::once(SymbolAttrs {
                name: "fn_deviant".into(),
                file: "src/test.rs".into(),
                attributes: vec!["kind:function".into(), "has_doc".into()],
                component_id: Some("comp".into()),
            }))
            .collect();

        let mut by_component = HashMap::new();
        by_component.insert("comp".into(), symbols);

        let changed = vec![SymbolAttrs {
            name: "fn_deviant".into(),
            file: "src/test.rs".into(),
            attributes: vec!["kind:function".into(), "has_doc".into()],
            component_id: Some("comp".into()),
        }];

        let deviations = detect_deviations(&changed, &by_component, &[], &[], &HashSet::new());

        // has_doc → returns_result would be obligation→obligation; must NOT appear
        let has_obligation_ante = deviations.iter().any(|d| {
            d.pattern_antecedent.iter().any(|a| {
                crate::conventions::classify_attribute(a)
                    == crate::conventions::AttributeRole::Obligation
            })
        });
        assert!(
            !has_obligation_ante,
            "obligation antecedent patterns must be excluded"
        );
    }

    #[test]
    fn detect_deviations_actionable_not_crowded_by_sketch() {
        let make_component = |prefix: &str, count: usize, comp: &str| -> Vec<SymbolAttrs> {
            (0..count)
                .map(|i| SymbolAttrs {
                    name: format!("{prefix}_{i}"),
                    file: format!("src/{prefix}.rs"),
                    attributes: vec!["kind:function".into(), "vis:pub".into(), "has_doc".into()],
                    component_id: Some(comp.into()),
                })
                .collect()
        };

        let mut by_component: HashMap<String, Vec<SymbolAttrs>> = HashMap::new();

        let mut sketch_syms = make_component("sketch", 10, "sketch_comp");
        sketch_syms.push(SymbolAttrs {
            name: "sketch_deviant".into(),
            file: "src/sketch.rs".into(),
            attributes: vec!["kind:function".into(), "vis:pub".into()],
            component_id: Some("sketch_comp".into()),
        });
        by_component.insert("sketch_comp".into(), sketch_syms);

        let mut stable_syms = make_component("stable", 10, "stable_comp");
        stable_syms.push(SymbolAttrs {
            name: "stable_deviant".into(),
            file: "src/stable.rs".into(),
            attributes: vec!["kind:function".into(), "vis:pub".into()],
            component_id: Some("stable_comp".into()),
        });
        by_component.insert("stable_comp".into(), stable_syms);

        let changed = vec![
            SymbolAttrs {
                name: "sketch_deviant".into(),
                file: "src/sketch.rs".into(),
                attributes: vec!["kind:function".into(), "vis:pub".into()],
                component_id: Some("sketch_comp".into()),
            },
            SymbolAttrs {
                name: "stable_deviant".into(),
                file: "src/stable.rs".into(),
                attributes: vec!["kind:function".into(), "vis:pub".into()],
                component_id: Some("stable_comp".into()),
            },
        ];

        let mut sketch_set = HashSet::new();
        sketch_set.insert("sketch_comp".into());

        let deviations = detect_deviations(&changed, &by_component, &[], &[], &sketch_set);

        let actionable: Vec<_> = deviations.iter().filter(|d| !d.informational).collect();
        assert!(
            !actionable.is_empty(),
            "actionable deviations must not be crowded out by sketch-mode"
        );
        assert_eq!(actionable[0].symbol, "stable_deviant");
    }
}
