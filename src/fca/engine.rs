use std::collections::HashMap;

use super::context::FormalContext;

#[derive(Clone)]
pub struct SymbolAttrs {
    pub name: String,
    pub attributes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Convention {
    pub id: String,
    pub antecedent: Vec<String>,
    pub consequent: Vec<String>,
    pub support: usize,
    pub confidence: f64,
}

impl Convention {
    pub fn compute_id(antecedent: &[String], consequent: &[String]) -> String {
        let mut ante = antecedent.to_vec();
        ante.sort();
        let mut cons = consequent.to_vec();
        cons.sort();
        let input = format!("{}\0{}", ante.join("\x1f"), cons.join("\x1f"));
        blake3::hash(input.as_bytes()).to_hex()[..16].to_string()
    }
}

const MIN_SUPPORT: usize = 3;
const MIN_CONFIDENCE: f64 = 0.9;

pub struct FcaEngine {
    context: Option<FormalContext>,
    conventions: Vec<Convention>,
    symbol_attrs: Vec<SymbolAttrs>,
}

impl FcaEngine {
    pub fn new() -> Self {
        Self {
            context: None,
            conventions: Vec::new(),
            symbol_attrs: Vec::new(),
        }
    }

    pub fn rebuild(&mut self, symbols: &[SymbolAttrs]) -> Vec<Convention> {
        let (ctx, attr_names) = Self::build_context(symbols);
        let impls = ctx.approximate_implications(MIN_SUPPORT, MIN_CONFIDENCE);

        self.conventions = impls
            .into_iter()
            .map(|imp| {
                let id = Convention::compute_id(&imp.antecedent, &imp.consequent);
                Convention {
                    id,
                    antecedent: imp.antecedent,
                    consequent: imp.consequent,
                    support: imp.support,
                    confidence: imp.confidence,
                }
            })
            .collect();

        self.context = Some(ctx);
        self.symbol_attrs = symbols.iter().map(|s| SymbolAttrs {
            name: s.name.clone(),
            attributes: s.attributes.clone(),
        }).collect();
        let _ = attr_names;

        self.conventions.clone()
    }

    pub fn update_incremental(
        &mut self,
        added: &[SymbolAttrs],
        removed: &[String],
    ) -> Vec<Convention> {
        self.symbol_attrs.retain(|s| !removed.contains(&s.name));
        for sa in added {
            self.symbol_attrs.push(SymbolAttrs {
                name: sa.name.clone(),
                attributes: sa.attributes.clone(),
            });
        }
        self.rebuild(&self.symbol_attrs.clone())
    }

    pub fn conventions(&self) -> &[Convention] {
        &self.conventions
    }

    fn build_context(symbols: &[SymbolAttrs]) -> (FormalContext, Vec<String>) {
        let mut attr_map: HashMap<String, usize> = HashMap::new();
        let mut relations: Vec<(usize, usize)> = Vec::new();

        for (obj_idx, sym) in symbols.iter().enumerate() {
            for attr in &sym.attributes {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_hash_same_inputs_same_output() {
        let id1 = Convention::compute_id(
            &["kind:function".into()],
            &["has_sig".into()],
        );
        let id2 = Convention::compute_id(
            &["kind:function".into()],
            &["has_sig".into()],
        );
        assert_eq!(id1, id2);
    }

    #[test]
    fn stable_hash_order_independent() {
        let id1 = Convention::compute_id(
            &["a".into(), "b".into()],
            &["c".into(), "d".into()],
        );
        let id2 = Convention::compute_id(
            &["b".into(), "a".into()],
            &["d".into(), "c".into()],
        );
        assert_eq!(id1, id2);
    }

    #[test]
    fn stable_hash_different_for_different_implications() {
        let id1 = Convention::compute_id(
            &["kind:function".into()],
            &["has_sig".into()],
        );
        let id2 = Convention::compute_id(
            &["kind:struct".into()],
            &["naming:CamelCase".into()],
        );
        assert_ne!(id1, id2);
    }

    #[test]
    fn rebuild_produces_conventions() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        let conventions = engine.rebuild(&symbols);
        assert!(!conventions.is_empty());
        let found = conventions.iter().any(|c| {
            c.antecedent == vec!["kind:function"] && c.consequent == vec!["has_sig"]
        });
        assert!(found, "expected kind:function → has_sig");
    }

    #[test]
    fn rebuild_produces_stable_ids_across_calls() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        let conv1 = engine.rebuild(&symbols);
        let conv2 = engine.rebuild(&symbols);
        let ids1: Vec<&str> = conv1.iter().map(|c| c.id.as_str()).collect();
        let ids2: Vec<&str> = conv2.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids1, ids2);
    }

    #[test]
    fn incremental_add_changes_conventions() {
        let mut symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        let before = engine.rebuild(&symbols);

        // Add another function WITHOUT has_sig → lowers confidence
        let extra = SymbolAttrs {
            name: "fn_extra".into(),
            attributes: vec!["kind:function".into()],
        };
        let after = engine.update_incremental(&[extra], &[]);

        let before_conf = before
            .iter()
            .find(|c| c.antecedent == vec!["kind:function"] && c.consequent == vec!["has_sig"])
            .map(|c| c.confidence);
        let after_conf = after
            .iter()
            .find(|c| c.antecedent == vec!["kind:function"] && c.consequent == vec!["has_sig"])
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

        // Remove the one function that lacks has_sig → confidence goes to 1.0, becomes exact → disappears
        let after = engine.update_incremental(&[], &["fn_9".into()]);
        let found = after.iter().any(|c| {
            c.antecedent == vec!["kind:function"] && c.consequent == vec!["has_sig"]
        });
        // At 1.0 confidence it's exact, not approximate — should be gone
        assert!(!found);
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
                attributes: attrs,
            });
        }
        // 5 structs with naming:CamelCase
        for i in 0..5 {
            symbols.push(SymbolAttrs {
                name: format!("Struct{i}"),
                attributes: vec!["kind:struct".into(), "naming:CamelCase".into()],
            });
        }
        symbols
    }
}
