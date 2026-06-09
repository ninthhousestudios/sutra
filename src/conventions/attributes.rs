use crate::db::{RefRow, SymbolRow};
use crate::parser::adapter::LanguageRegistry;

use super::engine::SymbolAttrs;

pub struct EffectPattern {
    pub attr_name: &'static str,
    pub callee_prefixes: &'static [&'static str],
}

pub struct ResolvedCallee {
    pub qualified_name: String,
    pub signature: Option<String>,
}

const MEANINGFUL_KINDS: &[&str] = &[
    "function",
    "method",
    "struct",
    "enum",
    "trait",
    "impl",
    "type_alias",
    "const",
];

pub fn extract_cross_language_attrs(sym: &SymbolRow, file_path: &str) -> Option<SymbolAttrs> {
    if sym.flags & 0x03 != 0 {
        return None;
    }
    if !MEANINGFUL_KINDS.contains(&sym.kind.as_str()) {
        return None;
    }

    let mut attributes = Vec::new();

    attributes.push(format!("kind:{}", sym.kind));

    match sym.visibility.as_deref() {
        Some("pub") => attributes.push("vis:pub".into()),
        Some("pub(crate)") => attributes.push("vis:pub_crate".into()),
        _ => attributes.push("vis:private".into()),
    }

    if sym.docstring.is_some() {
        attributes.push("has_doc".into());
    }

    if sym.signature.is_some() {
        attributes.push("has_sig".into());
    }

    if let Some(cog) = sym.cognitive {
        let bucket = if cog == 0 {
            "complexity:zero"
        } else if cog <= 5 {
            "complexity:low"
        } else if cog <= 15 {
            "complexity:med"
        } else {
            "complexity:high"
        };
        attributes.push(bucket.into());
    }

    let naming = if sym.short_name.len() > 1
        && sym.short_name.contains('_')
        && sym.short_name.chars().all(|c| c.is_uppercase() || c == '_')
    {
        "naming:SCREAMING"
    } else if sym
        .short_name
        .chars()
        .next()
        .is_some_and(|c| c.is_uppercase())
    {
        "naming:CamelCase"
    } else {
        "naming:snake_case"
    };
    attributes.push(naming.into());

    if sym.kind == "method" {
        attributes.push("is_method".into());
    }

    let parts: Vec<&str> = file_path.split('/').collect();
    if parts.len() >= 2 {
        let dir = if parts[0] == "src" && parts.len() >= 3 {
            format!("in:{}/{}", parts[0], parts[1])
        } else {
            format!("in:{}", parts[0])
        };
        attributes.push(dir);
    }

    Some(SymbolAttrs {
        name: sym.qualified_name.clone(),
        file: file_path.to_string(),
        attributes,
        component_id: None,
    })
}

pub fn extract_attrs_for_symbol(
    sym: &SymbolRow,
    file_path: &str,
    file_language: &str,
    registry: &LanguageRegistry,
) -> Option<SymbolAttrs> {
    if let Some(adapter) = registry.adapter_for_language(file_language)
        && let Some(fca_source) = adapter.as_fca_source()
    {
        return fca_source.extract_attributes(sym, file_path);
    }
    extract_cross_language_attrs(sym, file_path)
}

pub fn enrich_with_effects(
    sym_attrs: &mut SymbolAttrs,
    sym: &SymbolRow,
    call_refs: &[&RefRow],
    resolve_callee: &dyn Fn(i64) -> Option<ResolvedCallee>,
    patterns: &[EffectPattern],
) {
    let mut has_unsafe_callee = false;

    let resolved: Vec<_> = call_refs
        .iter()
        .filter_map(|r| r.target_symbol_id.and_then(resolve_callee))
        .collect();

    for pattern in patterns {
        if resolved.iter().any(|c| {
            pattern
                .callee_prefixes
                .iter()
                .any(|p| c.qualified_name.starts_with(p))
        }) {
            sym_attrs.attributes.push(pattern.attr_name.to_string());
        }
    }

    for callee in &resolved {
        if let Some(ref sig) = callee.signature
            && sig.contains("unsafe ")
        {
            has_unsafe_callee = true;
            break;
        }
    }
    if has_unsafe_callee {
        sym_attrs.attributes.push("effect:unsafe_transitive".into());
    }

    if let Some(ref sig) = sym.signature
        && sig.contains("&mut ")
    {
        sym_attrs.attributes.push("effect:mut_state".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_symbol(
        kind: &str,
        visibility: Option<&str>,
        signature: Option<&str>,
        docstring: Option<&str>,
        cognitive: Option<i64>,
        flags: i64,
    ) -> SymbolRow {
        SymbolRow {
            id: 1,
            file_id: 1,
            qualified_name: "mod::my_func".into(),
            short_name: "my_func".into(),
            kind: kind.into(),
            signature: signature.map(|s| s.into()),
            signature_hash: None,
            visibility: visibility.map(|v| v.into()),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: docstring.map(|d| d.into()),
            pagerank: None,
            cyclomatic: None,
            cognitive,
            max_nesting: None,
            flags,
            language_attrs: None,
        }
    }

    #[test]
    fn extracts_basic_function_attrs() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn my_func() -> Result<()>"),
            None,
            Some(3),
            0,
        );
        let sa = extract_cross_language_attrs(&sym, "src/tools/foo.rs").unwrap();
        assert!(sa.attributes.contains(&"kind:function".to_string()));
        assert!(sa.attributes.contains(&"vis:pub".to_string()));
        assert!(sa.attributes.contains(&"has_sig".to_string()));
        assert!(!sa.attributes.contains(&"returns_result".to_string()));
        assert!(sa.attributes.contains(&"naming:snake_case".to_string()));
        assert!(sa.attributes.contains(&"complexity:low".to_string()));
        assert!(sa.attributes.contains(&"in:src/tools".to_string()));
    }

    #[test]
    fn extracts_struct_attrs() {
        let sym = make_symbol("struct", Some("pub"), None, Some("A thing"), None, 0);
        let mut sym = sym;
        sym.short_name = "MyStruct".into();
        sym.qualified_name = "mod::MyStruct".into();
        let sa = extract_cross_language_attrs(&sym, "src/lib.rs").unwrap();
        assert!(sa.attributes.contains(&"kind:struct".to_string()));
        assert!(sa.attributes.contains(&"naming:CamelCase".to_string()));
        assert!(sa.attributes.contains(&"has_doc".to_string()));
        assert!(!sa.attributes.contains(&"has_sig".to_string()));
    }

    #[test]
    fn skips_test_symbols() {
        let sym = make_symbol("function", Some("pub"), Some("fn test()"), None, None, 0x01);
        assert!(extract_cross_language_attrs(&sym, "src/foo.rs").is_none());
    }

    #[test]
    fn skips_non_meaningful_kinds() {
        let sym = make_symbol("module", None, None, None, None, 0);
        assert!(extract_cross_language_attrs(&sym, "src/foo.rs").is_none());
    }

    #[test]
    fn method_gets_is_method_attr() {
        let sym = make_symbol("method", None, Some("fn do_it(&self)"), None, Some(0), 0);
        let sa = extract_cross_language_attrs(&sym, "src/foo.rs").unwrap();
        assert!(sa.attributes.contains(&"is_method".to_string()));
        assert!(!sa.attributes.contains(&"takes_self_ref".to_string()));
    }

    #[test]
    fn dispatch_with_rust_adapter_includes_language_attrs() {
        use crate::parser::adapter::default_registry;
        let registry = default_registry();
        let mut sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn my_func() -> Result<()>"),
            None,
            Some(3),
            0,
        );
        sym.language_attrs = Some(r#"{"returns_result":true}"#.into());
        let sa = extract_attrs_for_symbol(&sym, "src/tools/foo.rs", "rust", &registry).unwrap();
        assert!(sa.attributes.contains(&"kind:function".to_string()));
        assert!(sa.attributes.contains(&"vis:pub".to_string()));
        assert!(sa.attributes.contains(&"returns_result".to_string()));
        assert!(sa.attributes.contains(&"complexity:low".to_string()));
    }

    #[test]
    fn dispatch_dart_fca_source_includes_cross_language_attrs() {
        use crate::parser::adapter::default_registry;
        let registry = default_registry();
        let sym = make_symbol("function", Some("pub"), Some("fn foo()"), None, Some(0), 0);
        let sa = extract_attrs_for_symbol(&sym, "lib/foo.dart", "dart", &registry).unwrap();
        assert!(sa.attributes.contains(&"kind:function".to_string()));
        assert!(sa.attributes.contains(&"vis:pub".to_string()));
        assert!(!sa.attributes.contains(&"returns_result".to_string()));
    }

    fn make_ref(target_id: Option<i64>, line: i64) -> RefRow {
        RefRow {
            id: 1,
            file_id: 1,
            target_symbol_id: target_id,
            unresolved_name: None,
            line,
            col: 0,
            context_kind: "call".into(),
        }
    }

    fn resolve_with(name: &str, sig: Option<&str>) -> ResolvedCallee {
        ResolvedCallee {
            qualified_name: name.into(),
            signature: sig.map(|s| s.into()),
        }
    }

    #[test]
    fn effect_enrichment_matches_callee_prefix() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn read_file()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/io.rs").unwrap();
        let r = make_ref(Some(100), 5);
        let patterns = [EffectPattern {
            attr_name: "effect:fs",
            callee_prefixes: &["std::fs::"],
        }];
        enrich_with_effects(
            &mut attrs,
            &sym,
            &[&r],
            &|id| {
                (id == 100).then(|| {
                    resolve_with(
                        "std::fs::read_to_string",
                        Some("fn read_to_string(path: impl AsRef<Path>) -> Result<String>"),
                    )
                })
            },
            &patterns,
        );
        assert!(attrs.attributes.contains(&"effect:fs".to_string()));
    }

    #[test]
    fn effect_enrichment_no_match_no_attrs() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn compute()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/math.rs").unwrap();
        let r = make_ref(Some(200), 5);
        let patterns = [EffectPattern {
            attr_name: "effect:fs",
            callee_prefixes: &["std::fs::"],
        }];
        enrich_with_effects(
            &mut attrs,
            &sym,
            &[&r],
            &|_| Some(resolve_with("my_crate::util::helper", Some("fn helper()"))),
            &patterns,
        );
        assert!(!attrs.attributes.contains(&"effect:fs".to_string()));
    }

    #[test]
    fn effect_enrichment_multiple_patterns() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn sync_data()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/sync.rs").unwrap();
        let r1 = make_ref(Some(10), 3);
        let r2 = make_ref(Some(20), 7);
        let patterns = [
            EffectPattern {
                attr_name: "effect:fs",
                callee_prefixes: &["std::fs::"],
            },
            EffectPattern {
                attr_name: "effect:net",
                callee_prefixes: &["reqwest::"],
            },
        ];
        enrich_with_effects(
            &mut attrs,
            &sym,
            &[&r1, &r2],
            &|id| match id {
                10 => Some(resolve_with(
                    "std::fs::write",
                    Some(
                        "fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<()>",
                    ),
                )),
                20 => Some(resolve_with(
                    "reqwest::Client::get",
                    Some("fn get(&self, url: impl IntoUrl) -> RequestBuilder"),
                )),
                _ => None,
            },
            &patterns,
        );
        assert!(attrs.attributes.contains(&"effect:fs".to_string()));
        assert!(attrs.attributes.contains(&"effect:net".to_string()));
    }

    #[test]
    fn effect_unsafe_transitive_from_callee_signature() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn safe_wrapper()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/ffi.rs").unwrap();
        let r = make_ref(Some(50), 5);
        enrich_with_effects(
            &mut attrs,
            &sym,
            &[&r],
            &|id| {
                (id == 50).then(|| {
                    resolve_with(
                        "libc::malloc",
                        Some("unsafe fn malloc(size: usize) -> *mut c_void"),
                    )
                })
            },
            &[],
        );
        assert!(
            attrs
                .attributes
                .contains(&"effect:unsafe_transitive".to_string())
        );
    }

    #[test]
    fn effect_no_unsafe_transitive_for_safe_callee() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn do_stuff()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/lib.rs").unwrap();
        let r = make_ref(Some(60), 5);
        enrich_with_effects(
            &mut attrs,
            &sym,
            &[&r],
            &|id| {
                (id == 60).then(|| {
                    resolve_with("std::vec::Vec::push", Some("fn push(&mut self, value: T)"))
                })
            },
            &[],
        );
        assert!(
            !attrs
                .attributes
                .contains(&"effect:unsafe_transitive".to_string())
        );
    }

    #[test]
    fn effect_mut_state_from_signature() {
        let sym = make_symbol(
            "method",
            Some("pub"),
            Some("fn update(&mut self, val: i32)"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/state.rs").unwrap();
        enrich_with_effects(&mut attrs, &sym, &[], &|_| None, &[]);
        assert!(attrs.attributes.contains(&"effect:mut_state".to_string()));
    }

    #[test]
    fn effect_no_mut_state_for_immutable_ref() {
        let sym = make_symbol(
            "method",
            Some("pub"),
            Some("fn query(&self) -> Vec<String>"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/query.rs").unwrap();
        enrich_with_effects(&mut attrs, &sym, &[], &|_| None, &[]);
        assert!(!attrs.attributes.contains(&"effect:mut_state".to_string()));
    }
}
