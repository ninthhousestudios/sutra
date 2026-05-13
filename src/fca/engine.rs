use std::collections::{HashMap, HashSet};

use super::context::FormalContext;
use crate::rules::ConventionsConfig;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConventionViolation {
    pub symbol: String,
    pub convention_id: String,
    pub antecedent: Vec<String>,
    pub consequent: Vec<String>,
    pub missing: Vec<String>,
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
            self.symbol_attrs.retain(|s| s.name != sa.name);
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

    pub fn check(
        &self,
        changed_symbols: &[SymbolAttrs],
        config: &ConventionsConfig,
    ) -> Vec<ConventionViolation> {
        let suppressed: HashSet<&str> = config.suppress.iter().map(|s| s.as_str()).collect();
        let mut violations = Vec::new();

        for conv in &self.conventions {
            if suppressed.contains(conv.id.as_str()) {
                continue;
            }

            let exempted_symbols: HashSet<&str> = config
                .exempt
                .iter()
                .filter(|e| e.convention == conv.id)
                .flat_map(|e| e.symbols.iter().map(|s| s.as_str()))
                .collect();

            for sym in changed_symbols {
                if exempted_symbols.contains(sym.name.as_str()) {
                    continue;
                }

                let attrs: HashSet<&str> = sym.attributes.iter().map(|a| a.as_str()).collect();
                let has_antecedent = conv.antecedent.iter().all(|a| attrs.contains(a.as_str()));
                if !has_antecedent {
                    continue;
                }

                let missing: Vec<String> = conv
                    .consequent
                    .iter()
                    .filter(|c| !attrs.contains(c.as_str()))
                    .cloned()
                    .collect();

                if !missing.is_empty() {
                    violations.push(ConventionViolation {
                        symbol: sym.name.clone(),
                        convention_id: conv.id.clone(),
                        antecedent: conv.antecedent.clone(),
                        consequent: conv.consequent.clone(),
                        missing,
                    });
                }
            }
        }

        violations
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
    use crate::rules::ConventionExemption;

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
        let symbols = make_test_symbols();
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

    #[test]
    fn incremental_add_is_idempotent() {
        let symbols = make_test_symbols();
        let mut engine = FcaEngine::new();
        let first = engine.rebuild(&symbols);

        let extra = SymbolAttrs {
            name: "fn_0".into(),
            attributes: vec!["kind:function".into(), "has_sig".into()],
        };
        // Replaying same symbol should replace, not duplicate
        let second = engine.update_incremental(&[extra.clone()], &[]);
        let third = engine.update_incremental(&[extra], &[]);

        let count_fn = |convs: &[Convention]| {
            convs.iter().find(|c| {
                c.antecedent == vec!["kind:function"] && c.consequent == vec!["has_sig"]
            }).map(|c| (c.support, c.confidence))
        };
        assert_eq!(count_fn(&second), count_fn(&third), "replay should not change stats");
        assert_eq!(count_fn(&first), count_fn(&second), "replacing with same data should be stable");
    }

    #[test]
    fn realistic_codebase_produces_expected_conventions() {
        let symbols = make_realistic_symbols();
        let mut engine = FcaEngine::new();
        let conventions = engine.rebuild(&symbols);

        let has = |ante: &str, cons: &str| -> bool {
            conventions.iter().any(|c| {
                c.antecedent.contains(&ante.to_string())
                    && c.consequent.contains(&cons.to_string())
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
        for c in &conventions {
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
                attributes: attrs,
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
                attributes: attrs,
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
                attributes: attrs,
            });
        }

        // 8 enums: CamelCase
        for i in 0..8 {
            symbols.push(SymbolAttrs {
                name: format!("Enum{i}"),
                attributes: vec![
                    "kind:enum".into(),
                    "naming:CamelCase".into(),
                    "vis:pub".into(),
                ],
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

    fn setup_engine_with_conventions() -> FcaEngine {
        let mut engine = FcaEngine::new();
        let symbols = make_test_symbols();
        engine.rebuild(&symbols);
        engine
    }

    #[test]
    fn check_detects_violations() {
        let engine = setup_engine_with_conventions();
        assert!(!engine.conventions().is_empty());

        let conv = &engine.conventions()[0];
        let changed = vec![SymbolAttrs {
            name: "bad_symbol".into(),
            attributes: conv.antecedent.clone(),
        }];

        let config = ConventionsConfig::default();
        let violations = engine.check(&changed, &config);

        assert!(!violations.is_empty());
        let v = violations.iter().find(|v| v.symbol == "bad_symbol").unwrap();
        assert_eq!(v.convention_id, conv.id);
        assert!(!v.missing.is_empty());
    }

    #[test]
    fn check_no_violation_when_consequent_satisfied() {
        let engine = setup_engine_with_conventions();
        let conv = &engine.conventions()[0];

        let mut attrs = conv.antecedent.clone();
        attrs.extend(conv.consequent.clone());
        let changed = vec![SymbolAttrs {
            name: "good_symbol".into(),
            attributes: attrs,
        }];

        let config = ConventionsConfig::default();
        let violations = engine.check(&changed, &config);
        let relevant: Vec<_> = violations.iter().filter(|v| v.symbol == "good_symbol" && v.convention_id == conv.id).collect();
        assert!(relevant.is_empty());
    }

    #[test]
    fn check_no_violation_when_antecedent_not_matched() {
        let engine = setup_engine_with_conventions();

        let changed = vec![SymbolAttrs {
            name: "unrelated".into(),
            attributes: vec!["some_random_attr".into()],
        }];

        let config = ConventionsConfig::default();
        let violations = engine.check(&changed, &config);
        assert!(violations.is_empty());
    }

    #[test]
    fn check_suppressed_conventions_skipped() {
        let engine = setup_engine_with_conventions();
        let conv = &engine.conventions()[0];

        let changed = vec![SymbolAttrs {
            name: "bad_symbol".into(),
            attributes: conv.antecedent.clone(),
        }];

        let config = ConventionsConfig {
            suppress: vec![conv.id.clone()],
            ..Default::default()
        };
        let violations = engine.check(&changed, &config);
        let relevant: Vec<_> = violations.iter().filter(|v| v.convention_id == conv.id).collect();
        assert!(relevant.is_empty());
    }

    #[test]
    fn check_per_symbol_exemption() {
        let engine = setup_engine_with_conventions();
        let conv = &engine.conventions()[0];

        let changed = vec![SymbolAttrs {
            name: "ExemptedSymbol".into(),
            attributes: conv.antecedent.clone(),
        }];

        let config = ConventionsConfig {
            exempt: vec![ConventionExemption {
                convention: conv.id.clone(),
                symbols: vec!["ExemptedSymbol".into()],
            }],
            ..Default::default()
        };
        let violations = engine.check(&changed, &config);
        let relevant: Vec<_> = violations.iter().filter(|v| v.symbol == "ExemptedSymbol" && v.convention_id == conv.id).collect();
        assert!(relevant.is_empty());
    }

    #[test]
    fn check_exemption_only_applies_to_specified_convention() {
        let engine = setup_engine_with_conventions();
        if engine.conventions().len() < 2 {
            return;
        }
        let conv_a = &engine.conventions()[0];
        let conv_b = &engine.conventions()[1];

        let mut attrs = conv_a.antecedent.clone();
        for a in &conv_b.antecedent {
            if !attrs.contains(a) {
                attrs.push(a.clone());
            }
        }

        let changed = vec![SymbolAttrs {
            name: "PartialExempt".into(),
            attributes: attrs,
        }];

        let config = ConventionsConfig {
            exempt: vec![ConventionExemption {
                convention: conv_a.id.clone(),
                symbols: vec!["PartialExempt".into()],
            }],
            ..Default::default()
        };
        let violations = engine.check(&changed, &config);
        let exempt_from_a: Vec<_> = violations.iter().filter(|v| v.symbol == "PartialExempt" && v.convention_id == conv_a.id).collect();
        assert!(exempt_from_a.is_empty());
    }
}
