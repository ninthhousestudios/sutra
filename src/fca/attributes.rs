use crate::db::SymbolRow;

use super::engine::SymbolAttrs;

const MEANINGFUL_KINDS: &[&str] = &[
    "function", "method", "struct", "enum", "trait", "impl", "type_alias", "const",
];

pub fn extract_symbol_attrs(sym: &SymbolRow, file_path: &str) -> Option<SymbolAttrs> {
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

    if let Some(ref sig) = sym.signature {
        if sig.contains("Result") {
            attributes.push("returns_result".into());
        }
        if sig.contains("Option") {
            attributes.push("returns_option".into());
        }
        if sig.contains("&self") {
            attributes.push("takes_self_ref".into());
        }
        if sig.contains("&mut self") {
            attributes.push("takes_self_mut".into());
        }
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
        && sym
            .short_name
            .chars()
            .all(|c| c.is_uppercase() || c == '_')
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
        attributes,
    })
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
            flags,
        }
    }

    #[test]
    fn extracts_basic_function_attrs() {
        let sym = make_symbol("function", Some("pub"), Some("fn my_func() -> Result<()>"), None, Some(3), 0);
        let sa = extract_symbol_attrs(&sym, "src/tools/foo.rs").unwrap();
        assert!(sa.attributes.contains(&"kind:function".to_string()));
        assert!(sa.attributes.contains(&"vis:pub".to_string()));
        assert!(sa.attributes.contains(&"has_sig".to_string()));
        assert!(sa.attributes.contains(&"returns_result".to_string()));
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
        let sa = extract_symbol_attrs(&sym, "src/lib.rs").unwrap();
        assert!(sa.attributes.contains(&"kind:struct".to_string()));
        assert!(sa.attributes.contains(&"naming:CamelCase".to_string()));
        assert!(sa.attributes.contains(&"has_doc".to_string()));
        assert!(!sa.attributes.contains(&"has_sig".to_string()));
    }

    #[test]
    fn skips_test_symbols() {
        let sym = make_symbol("function", Some("pub"), Some("fn test()"), None, None, 0x01);
        assert!(extract_symbol_attrs(&sym, "src/foo.rs").is_none());
    }

    #[test]
    fn skips_non_meaningful_kinds() {
        let sym = make_symbol("module", None, None, None, None, 0);
        assert!(extract_symbol_attrs(&sym, "src/foo.rs").is_none());
    }

    #[test]
    fn method_gets_is_method_attr() {
        let sym = make_symbol("method", None, Some("fn do_it(&self)"), None, Some(0), 0);
        let sa = extract_symbol_attrs(&sym, "src/foo.rs").unwrap();
        assert!(sa.attributes.contains(&"is_method".to_string()));
        assert!(sa.attributes.contains(&"takes_self_ref".to_string()));
    }
}
