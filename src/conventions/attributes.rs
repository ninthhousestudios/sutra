use std::collections::{HashMap, HashSet};

use crate::db::{RefRow, SymbolRow};
use crate::parser::adapter::LanguageRegistry;

use super::SymbolAttrs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributeRole {
    Identity,
    Obligation,
}

/// `vis:*` is Identity (antecedent-only) — it describes exposure, not a
/// prescriptive obligation.
pub fn classify_attribute(attr: &str) -> AttributeRole {
    match attr {
        "has_doc" | "is_async" | "returns_result" | "returns_option" | "returns_future" => {
            AttributeRole::Obligation
        }
        a if a.starts_with("naming:") => AttributeRole::Obligation,
        a if a.starts_with("effect:") => AttributeRole::Obligation,
        _ => AttributeRole::Identity,
    }
}

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
    if !MEANINGFUL_KINDS.contains(&&*sym.kind) {
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

    if &*sym.kind == "method" {
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
        name: sym.qualified_name.to_string(),
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
            pattern.callee_prefixes.iter().any(|p| {
                c.qualified_name.starts_with(p)
                    && (c.qualified_name.len() == p.len()
                        || p.ends_with("::")
                        || c.qualified_name.as_bytes().get(p.len()) == Some(&b':'))
            })
        }) {
            sym_attrs.attributes.push(pattern.attr_name.to_string());
        }
    }

    for pattern in patterns {
        if sym_attrs
            .attributes
            .contains(&pattern.attr_name.to_string())
        {
            continue;
        }
        if call_refs.iter().any(|r| {
            r.target_symbol_id.is_none()
                && r.unresolved_name.as_ref().is_some_and(|name| {
                    pattern.callee_prefixes.iter().any(|p| {
                        name.starts_with(p)
                            && (name.len() == p.len()
                                || name.as_bytes().get(p.len()) == Some(&b'.'))
                    })
                })
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

fn parse_dart_callee_prefix(prefix: &str) -> Option<(&str, Option<&str>)> {
    let mut parts = prefix.splitn(2, "::");
    let package = parts.next().filter(|s| !s.is_empty())?;
    let member = parts.next().filter(|s| !s.is_empty());
    Some((package, member))
}

/// Dart-specific: detect effects from import-derived package refs.
/// Pattern A: aliased calls (`http.get`) — FieldAccess ref name matches package name.
/// Pattern B: direct class use (`File.open`) — unresolved ref name matches class from package.
pub fn enrich_with_dart_import_effects(
    sym_attrs: &mut SymbolAttrs,
    sym: &SymbolRow,
    file_refs: &[RefRow],
    import_packages: &HashSet<&str>,
    patterns: &[EffectPattern],
) {
    for pattern in patterns {
        if sym_attrs
            .attributes
            .contains(&pattern.attr_name.to_string())
        {
            continue;
        }

        let matched = pattern.callee_prefixes.iter().any(|prefix| {
            let Some((package, member)) = parse_dart_callee_prefix(prefix) else {
                return false;
            };
            if !import_packages.contains(package) {
                return false;
            }
            file_refs.iter().any(|r| {
                if r.line < sym.start_line || r.line > sym.end_line {
                    return false;
                }
                let Some(ref name) = r.unresolved_name else {
                    return false;
                };
                match member {
                    None => r.context_kind == "field_access" && name == package,
                    Some(class) => {
                        matches!(
                            r.context_kind.as_str(),
                            "call" | "construction" | "field_access"
                        ) && name == class
                    }
                }
            })
        });

        if matched {
            sym_attrs.attributes.push(pattern.attr_name.to_string());
        }
    }
}

/// Shared enrichment: resolved-callee effects + Dart import-based effects.
/// Both `conventions/pipeline.rs` rebuild and `tools/review.rs` review-time
/// attribute building must call this to stay in sync.
pub fn enrich_all_effects(
    attrs: &mut SymbolAttrs,
    sym: &SymbolRow,
    file_refs: &[RefRow],
    callee_cache: &HashMap<i64, ResolvedCallee>,
    fca_source: &dyn crate::parser::adapter::FcaAttributeSource,
    dart_import_packages: Option<&HashSet<String>>,
) {
    let call_refs: Vec<_> = file_refs
        .iter()
        .filter(|r| r.context_kind == "call" && r.line >= sym.start_line && r.line <= sym.end_line)
        .collect();
    enrich_with_effects(
        attrs,
        sym,
        &call_refs,
        &|id| {
            callee_cache.get(&id).map(|c| ResolvedCallee {
                qualified_name: c.qualified_name.clone(),
                signature: c.signature.clone(),
            })
        },
        fca_source.effect_patterns(),
    );
    if let Some(pkgs) = dart_import_packages {
        let pkg_refs: HashSet<&str> = pkgs.iter().map(|s| s.as_str()).collect();
        enrich_with_dart_import_effects(
            attrs,
            sym,
            file_refs,
            &pkg_refs,
            fca_source.effect_patterns(),
        );
    }
}

/// Build the set of effect-relevant Dart package names from a file's imports.
pub fn dart_effect_packages(imports: &[crate::db::ImportRow]) -> Option<HashSet<String>> {
    let pkgs: HashSet<String> = imports
        .iter()
        .filter_map(|imp| {
            crate::constraints::external::external_crate_of_import(&imp.imported_path, "dart", &[])
        })
        .collect();
    if pkgs.is_empty() { None } else { Some(pkgs) }
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
            structural_hash: None,
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
            resolved_local_target: None,
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
    fn effect_bare_name_exact_match_only() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("void cleanup()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/mem.c").unwrap();
        let r1 = make_ref(Some(10), 3);
        let r2 = make_ref(Some(20), 5);
        let patterns = [EffectPattern {
            attr_name: "effect:heap",
            callee_prefixes: &["free", "malloc"],
        }];
        enrich_with_effects(
            &mut attrs,
            &sym,
            &[&r1, &r2],
            &|id| match id {
                10 => Some(resolve_with("free", None)),
                20 => Some(resolve_with("free_list", None)),
                _ => None,
            },
            &patterns,
        );
        assert!(attrs.attributes.contains(&"effect:heap".to_string()));

        // Now test that a function only calling free_list does NOT get the tag
        let mut attrs2 = extract_cross_language_attrs(&sym, "src/mem.c").unwrap();
        enrich_with_effects(
            &mut attrs2,
            &sym,
            &[&r2],
            &|id| (id == 20).then(|| resolve_with("free_list", None)),
            &patterns,
        );
        assert!(!attrs2.attributes.contains(&"effect:heap".to_string()));
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

    #[test]
    fn parse_dart_callee_prefix_package_alias() {
        assert_eq!(parse_dart_callee_prefix("http::"), Some(("http", None)));
        assert_eq!(parse_dart_callee_prefix("dio::"), Some(("dio", None)));
        assert_eq!(
            parse_dart_callee_prefix("sqflite::"),
            Some(("sqflite", None))
        );
    }

    #[test]
    fn parse_dart_callee_prefix_class_member() {
        assert_eq!(
            parse_dart_callee_prefix("dart:io::File"),
            Some(("dart:io", Some("File")))
        );
        assert_eq!(
            parse_dart_callee_prefix("dart:io::HttpClient"),
            Some(("dart:io", Some("HttpClient")))
        );
    }

    #[test]
    fn parse_dart_callee_prefix_empty() {
        assert_eq!(parse_dart_callee_prefix(""), None);
        assert_eq!(parse_dart_callee_prefix("::"), None);
    }

    fn make_dart_ref(name: &str, context_kind: &str, line: i64) -> RefRow {
        RefRow {
            id: 1,
            file_id: 1,
            target_symbol_id: None,
            unresolved_name: Some(name.into()),
            line,
            col: 0,
            context_kind: context_kind.into(),
            resolved_local_target: None,
        }
    }

    #[test]
    fn dart_import_effects_aliased_call() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn fetch_data()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "lib/api.dart").unwrap();
        let refs = vec![make_dart_ref("http", "field_access", 5)];
        let packages: HashSet<&str> = ["http"].into_iter().collect();
        let patterns = [EffectPattern {
            attr_name: "effect:net",
            callee_prefixes: &["http::", "dio::"],
        }];
        enrich_with_dart_import_effects(&mut attrs, &sym, &refs, &packages, &patterns);
        assert!(attrs.attributes.contains(&"effect:net".to_string()));
    }

    #[test]
    fn dart_import_effects_direct_class() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn read_file()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "lib/io.dart").unwrap();
        let refs = vec![make_dart_ref("File", "call", 3)];
        let packages: HashSet<&str> = ["dart:io"].into_iter().collect();
        let patterns = [EffectPattern {
            attr_name: "effect:fs",
            callee_prefixes: &["dart:io::File", "dart:io::Directory"],
        }];
        enrich_with_dart_import_effects(&mut attrs, &sym, &refs, &packages, &patterns);
        assert!(attrs.attributes.contains(&"effect:fs".to_string()));
    }

    #[test]
    fn dart_import_effects_no_match_without_import() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn helper()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "lib/util.dart").unwrap();
        let refs = vec![make_dart_ref("http", "field_access", 5)];
        let packages: HashSet<&str> = HashSet::new();
        let patterns = [EffectPattern {
            attr_name: "effect:net",
            callee_prefixes: &["http::"],
        }];
        enrich_with_dart_import_effects(&mut attrs, &sym, &refs, &packages, &patterns);
        assert!(!attrs.attributes.contains(&"effect:net".to_string()));
    }

    #[test]
    fn dart_import_effects_respects_symbol_span() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn no_io()"),
            None,
            Some(1),
            0,
        );
        // Symbol spans lines 1-10; ref is at line 15 (outside)
        let mut attrs = extract_cross_language_attrs(&sym, "lib/app.dart").unwrap();
        let refs = vec![make_dart_ref("http", "field_access", 15)];
        let packages: HashSet<&str> = ["http"].into_iter().collect();
        let patterns = [EffectPattern {
            attr_name: "effect:net",
            callee_prefixes: &["http::"],
        }];
        enrich_with_dart_import_effects(&mut attrs, &sym, &refs, &packages, &patterns);
        assert!(!attrs.attributes.contains(&"effect:net".to_string()));
    }

    #[test]
    fn dart_import_effects_skips_already_tagged() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn fetch()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "lib/net.dart").unwrap();
        attrs.attributes.push("effect:net".to_string());
        let refs = vec![make_dart_ref("http", "field_access", 5)];
        let packages: HashSet<&str> = ["http"].into_iter().collect();
        let patterns = [EffectPattern {
            attr_name: "effect:net",
            callee_prefixes: &["http::"],
        }];
        enrich_with_dart_import_effects(&mut attrs, &sym, &refs, &packages, &patterns);
        let count = attrs
            .attributes
            .iter()
            .filter(|a| *a == "effect:net")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn dart_import_effects_pattern_b_excludes_type_use() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn accepts_file()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "lib/typed.dart").unwrap();
        let refs = vec![make_dart_ref("File", "type_use", 3)];
        let packages: HashSet<&str> = ["dart:io"].into_iter().collect();
        let patterns = [EffectPattern {
            attr_name: "effect:fs",
            callee_prefixes: &["dart:io::File"],
        }];
        enrich_with_dart_import_effects(&mut attrs, &sym, &refs, &packages, &patterns);
        assert!(!attrs.attributes.contains(&"effect:fs".to_string()));
    }

    #[test]
    fn dart_import_effects_pattern_b_allows_construction() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("fn create_file()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "lib/io.dart").unwrap();
        let refs = vec![make_dart_ref("File", "construction", 5)];
        let packages: HashSet<&str> = ["dart:io"].into_iter().collect();
        let patterns = [EffectPattern {
            attr_name: "effect:fs",
            callee_prefixes: &["dart:io::File"],
        }];
        enrich_with_dart_import_effects(&mut attrs, &sym, &refs, &packages, &patterns);
        assert!(attrs.attributes.contains(&"effect:fs".to_string()));
    }

    fn make_unresolved_ref(name: &str, line: i64) -> RefRow {
        RefRow {
            id: 1,
            file_id: 1,
            target_symbol_id: None,
            unresolved_name: Some(name.into()),
            line,
            col: 0,
            context_kind: "call".into(),
            resolved_local_target: None,
        }
    }

    #[test]
    fn unresolved_ref_fallback_matches_python_effects() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("def do_stuff()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/app.py").unwrap();
        let r1 = make_unresolved_ref("open", 3);
        let r2 = make_unresolved_ref("requests.get", 5);
        let r3 = make_unresolved_ref("subprocess.run", 7);
        let patterns = [
            EffectPattern {
                attr_name: "effect:fs",
                callee_prefixes: &["open"],
            },
            EffectPattern {
                attr_name: "effect:net",
                callee_prefixes: &["requests"],
            },
            EffectPattern {
                attr_name: "effect:process",
                callee_prefixes: &["subprocess"],
            },
        ];
        enrich_with_effects(&mut attrs, &sym, &[&r1, &r2, &r3], &|_| None, &patterns);
        assert!(attrs.attributes.contains(&"effect:fs".to_string()));
        assert!(attrs.attributes.contains(&"effect:net".to_string()));
        assert!(attrs.attributes.contains(&"effect:process".to_string()));
    }

    #[test]
    fn unresolved_ref_fallback_dedup_with_resolved() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("def read()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/io.py").unwrap();
        let resolved = make_ref(Some(100), 3);
        let unresolved = make_unresolved_ref("open", 5);
        let patterns = [EffectPattern {
            attr_name: "effect:fs",
            callee_prefixes: &["open"],
        }];
        enrich_with_effects(
            &mut attrs,
            &sym,
            &[&resolved, &unresolved],
            &|id| (id == 100).then(|| resolve_with("open", Some("def open()"))),
            &patterns,
        );
        let count = attrs
            .attributes
            .iter()
            .filter(|a| *a == "effect:fs")
            .count();
        assert_eq!(count, 1);
    }

    #[test]
    fn unresolved_ref_fallback_respects_dot_boundary() {
        let sym = make_symbol(
            "function",
            Some("pub"),
            Some("def helper()"),
            None,
            Some(1),
            0,
        );
        let mut attrs = extract_cross_language_attrs(&sym, "src/util.py").unwrap();
        let r = make_unresolved_ref("open_file", 3);
        let patterns = [EffectPattern {
            attr_name: "effect:fs",
            callee_prefixes: &["open"],
        }];
        enrich_with_effects(&mut attrs, &sym, &[&r], &|_| None, &patterns);
        assert!(!attrs.attributes.contains(&"effect:fs".to_string()));
    }

    #[test]
    fn classify_identity_attributes() {
        assert_eq!(classify_attribute("kind:function"), AttributeRole::Identity);
        assert_eq!(classify_attribute("kind:struct"), AttributeRole::Identity);
        assert_eq!(classify_attribute("vis:pub"), AttributeRole::Identity);
        assert_eq!(classify_attribute("vis:private"), AttributeRole::Identity);
        assert_eq!(classify_attribute("vis:pub_crate"), AttributeRole::Identity);
        assert_eq!(classify_attribute("in:src/db"), AttributeRole::Identity);
        assert_eq!(classify_attribute("is_method"), AttributeRole::Identity);
        assert_eq!(classify_attribute("has_sig"), AttributeRole::Identity);
        assert_eq!(
            classify_attribute("complexity:high"),
            AttributeRole::Identity
        );
        assert_eq!(
            classify_attribute("complexity:zero"),
            AttributeRole::Identity
        );
    }

    #[test]
    fn classify_obligation_attributes() {
        assert_eq!(classify_attribute("has_doc"), AttributeRole::Obligation);
        assert_eq!(
            classify_attribute("naming:snake_case"),
            AttributeRole::Obligation
        );
        assert_eq!(
            classify_attribute("naming:CamelCase"),
            AttributeRole::Obligation
        );
        assert_eq!(
            classify_attribute("naming:SCREAMING"),
            AttributeRole::Obligation
        );
        assert_eq!(
            classify_attribute("returns_result"),
            AttributeRole::Obligation
        );
        assert_eq!(
            classify_attribute("returns_option"),
            AttributeRole::Obligation
        );
        assert_eq!(classify_attribute("is_async"), AttributeRole::Obligation);
        assert_eq!(classify_attribute("effect:io"), AttributeRole::Obligation);
        assert_eq!(classify_attribute("effect:db"), AttributeRole::Obligation);
        assert_eq!(
            classify_attribute("effect:mut_state"),
            AttributeRole::Obligation
        );
        assert_eq!(
            classify_attribute("effect:unsafe_transitive"),
            AttributeRole::Obligation
        );
        assert_eq!(
            classify_attribute("returns_future"),
            AttributeRole::Obligation
        );
    }
}
