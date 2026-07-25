use crate::error::Result;
use crate::parser::adapter::ParseContext;
use crate::parser::{
    ExtractedImport, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, SymbolKind,
    complexity, structural_hash,
};
use tree_sitter::{Node, TreeCursor};

pub fn parse(ctx: &ParseContext) -> Result<ParseResult> {
    let root = ctx.tree.root_node();
    let parsed_ok = !root.has_error();
    let src = ctx.source;
    let file_path = ctx.file_path;

    let symbols = collect_symbols(root, src, file_path);

    let mut references = Vec::new();
    collect_references(&mut references, root, src);

    let imports = collect_includes(root, src);

    Ok(ParseResult {
        file_path: file_path.to_string(),
        language: "c".to_string(),
        symbols,
        references,
        imports,
        parsed_ok,
        line_count: std::str::from_utf8(src)
            .map(|s| s.lines().count())
            .unwrap_or(0),
    })
}

// ---------------------------------------------------------------------------
// Symbol flags
// ---------------------------------------------------------------------------

pub const FLAG_TEST: u32 = 0x01;
pub const FLAG_FFI_ENTRY: u32 = 0x04;

fn extract_flags(file_path: &str, name: &str, node: Node) -> u32 {
    let mut flags = 0u32;

    if is_test_file(file_path) {
        flags |= FLAG_TEST;
    }

    if name.starts_with("test_") {
        flags |= FLAG_TEST;
    }

    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "attribute_specifier" | "attribute_declaration" => {
                flags |= FLAG_FFI_ENTRY;
            }
            "comment" => {}
            _ => break,
        }
        sib = s.prev_sibling();
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "attribute_specifier" || child.kind() == "attribute_declaration" {
            flags |= FLAG_FFI_ENTRY;
        }
    }

    flags
}

/// Whether `path` is C test code for constraint purposes: the unity/check-style
/// file-naming conventions [`is_test_file`] knows, plus any `test/` or `tests/`
/// directory. Split for the same reason as Python's — `is_test_file` drives
/// symbol `FLAG_TEST`, and a directory is a weaker claim than a file name
/// (sutra/295).
pub fn is_test_path(path: &str) -> bool {
    is_test_file(path) || crate::parser::adapter::path_in_test_dir(path)
}

fn is_test_file(path: &str) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let lower = file_name.to_ascii_lowercase();
    lower.ends_with("_test.c")
        || lower.starts_with("test_")
        || path.contains("/tests/")
        || path.starts_with("tests/")
}

fn is_header_guard(name: &str) -> bool {
    name.ends_with("_H")
        || name.ends_with("_H_")
        || name.ends_with("_INCLUDED")
        || name.ends_with("_INCLUDED_")
}

// ---------------------------------------------------------------------------
// Symbol extraction
// ---------------------------------------------------------------------------

fn collect_symbols(node: Node, src: &[u8], file_path: &str) -> Vec<ExtractedSymbol> {
    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_definition" => {
                if let Some(sym) = extract_function(child, src, file_path) {
                    symbols.push(sym);
                }
            }
            "struct_specifier" if child.child_by_field_name("body").is_some() => {
                if let Some(sym) = extract_struct(child, src, None) {
                    symbols.push(sym);
                }
            }
            "enum_specifier" if child.child_by_field_name("body").is_some() => {
                if let Some(sym) = extract_enum(child, src, None) {
                    symbols.push(sym);
                }
            }
            "type_definition" => {
                if let Some(type_node) = child.child_by_field_name("type") {
                    let has_name = type_node.child_by_field_name("name").is_some();
                    let has_body = type_node.child_by_field_name("body").is_some();
                    if has_name && has_body {
                        match type_node.kind() {
                            "struct_specifier" => {
                                if let Some(sym) = extract_struct(type_node, src, Some(child)) {
                                    symbols.push(sym);
                                }
                            }
                            "enum_specifier" => {
                                if let Some(sym) = extract_enum(type_node, src, Some(child)) {
                                    symbols.push(sym);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                collect_typedef_declarators(child, src, &mut symbols);
            }
            "declaration" => {
                if let Some(type_node) = child.child_by_field_name("type")
                    && type_node.child_by_field_name("body").is_some()
                {
                    match type_node.kind() {
                        "struct_specifier" => {
                            if let Some(sym) = extract_struct(type_node, src, Some(child)) {
                                symbols.push(sym);
                            }
                        }
                        "enum_specifier" => {
                            if let Some(sym) = extract_enum(type_node, src, Some(child)) {
                                symbols.push(sym);
                            }
                        }
                        _ => {}
                    }
                }
                if !has_specifier(child, src, "extern") {
                    collect_var_declarators(child, src, file_path, &mut symbols);
                }
            }
            "preproc_function_def" => {
                if let Some(sym) = extract_macro(child, src, file_path) {
                    symbols.push(sym);
                }
            }
            "preproc_def" => {
                if let Some(sym) = extract_const_define(child, src, file_path) {
                    symbols.push(sym);
                }
            }
            _ => {}
        }
    }
    symbols
}

fn extract_function(node: Node, src: &[u8], file_path: &str) -> Option<ExtractedSymbol> {
    let declarator = node.child_by_field_name("declarator")?;
    let func_decl = find_function_declarator(declarator)?;
    let name_decl = func_decl.child_by_field_name("declarator")?;
    let name = find_name_in_declarator(name_decl, src)?;
    let name_ident = find_name_node_in_declarator(name_decl);
    let sh = Some(structural_hash::compute(
        node,
        src,
        name_ident.map(|n| (n.start_byte(), n.end_byte())),
    ));

    let visibility = extract_visibility(node, src);
    let docstring = extract_docstring(node, src);
    let signature = build_fn_signature(node, src);
    let signature_hash = signature
        .as_ref()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
    let language_attrs = extract_fn_language_attrs(node, src, declarator);
    let flags = extract_flags(file_path, &name, node);

    let (cyclomatic, cognitive, max_nesting) = if let Some(body) = node.child_by_field_name("body")
    {
        (
            Some(complexity::cyclomatic(body, src, "c")),
            Some(complexity::cognitive(body, src, "c")),
            Some(complexity::max_nesting_depth(body, src, "c")),
        )
    } else {
        (Some(1), Some(0), Some(0))
    };

    Some(ExtractedSymbol {
        qualified_name: name.clone(),
        short_name: name,
        kind: SymbolKind::Function,
        signature,
        signature_hash,
        structural_hash: sh,
        visibility,
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        children: vec![],
        parent_symbol_id: None,
        docstring,
        cyclomatic,
        cognitive,
        max_nesting,
        flags,
        language_attrs,
    })
}

fn extract_struct(node: Node, src: &[u8], doc_anchor: Option<Node>) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(src).ok()?.to_string();
    let docstring = extract_docstring(doc_anchor.unwrap_or(node), src);
    let sh = Some(structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    ));

    let children = node
        .child_by_field_name("body")
        .map(|body| extract_struct_fields(body, src, &name))
        .unwrap_or_default();

    Some(ExtractedSymbol {
        qualified_name: name.clone(),
        short_name: name,
        kind: SymbolKind::Struct,
        signature: None,
        signature_hash: None,
        structural_hash: sh,
        visibility: Some("pub".to_string()),
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        children,
        parent_symbol_id: None,
        docstring,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    })
}

fn extract_struct_fields(body: Node, src: &[u8], struct_name: &str) -> Vec<ExtractedSymbol> {
    let mut fields = Vec::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let Some(field_name) = find_field_identifier(child, src) else {
            continue;
        };
        let type_text = child
            .child_by_field_name("type")
            .and_then(|t| t.utf8_text(src).ok());
        let signature = type_text.map(|t| format!("{t} {field_name}"));
        let signature_hash = signature
            .as_ref()
            .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
        let docstring = extract_docstring(child, src);

        let field_ident = find_field_identifier_node(child);
        let sh = Some(structural_hash::compute(
            child,
            src,
            field_ident.map(|n| (n.start_byte(), n.end_byte())),
        ));

        fields.push(ExtractedSymbol {
            qualified_name: format!("{struct_name}::{field_name}"),
            short_name: field_name,
            kind: SymbolKind::Field,
            signature,
            signature_hash,
            structural_hash: sh,
            visibility: Some("pub".to_string()),
            start_line: child.start_position().row + 1,
            start_col: child.start_position().column,
            end_line: child.end_position().row + 1,
            end_col: child.end_position().column,
            children: vec![],
            parent_symbol_id: None,
            docstring,
            cyclomatic: None,
            cognitive: None,
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        });
    }
    fields
}

fn find_field_identifier(node: Node, src: &[u8]) -> Option<String> {
    find_field_identifier_node(node).and_then(|n| n.utf8_text(src).ok().map(|s| s.to_string()))
}

fn find_field_identifier_node(node: Node) -> Option<Node> {
    if node.kind() == "field_identifier" {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(n) = find_field_identifier_node(child) {
            return Some(n);
        }
    }
    None
}

fn extract_enum(node: Node, src: &[u8], doc_anchor: Option<Node>) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(src).ok()?.to_string();
    let docstring = extract_docstring(doc_anchor.unwrap_or(node), src);
    let sh = Some(structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    ));

    Some(ExtractedSymbol {
        qualified_name: name.clone(),
        short_name: name,
        kind: SymbolKind::Enum,
        signature: None,
        signature_hash: None,
        structural_hash: sh,
        visibility: Some("pub".to_string()),
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        children: vec![],
        parent_symbol_id: None,
        docstring,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    })
}

fn collect_typedef_declarators(node: Node, src: &[u8], symbols: &mut Vec<ExtractedSymbol>) {
    let docstring = extract_docstring(node, src);
    let signature = node
        .utf8_text(src)
        .ok()
        .map(|s| s.trim_end_matches(';').trim().to_string());
    let signature_hash = signature
        .as_ref()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());

    for declarator in field_children(node, "declarator") {
        if let Some(name) = find_name_in_declarator(declarator, src) {
            let name_ident = find_name_node_in_declarator(declarator);
            let sh = Some(structural_hash::compute(
                node,
                src,
                name_ident.map(|n| (n.start_byte(), n.end_byte())),
            ));
            symbols.push(ExtractedSymbol {
                qualified_name: name.clone(),
                short_name: name,
                kind: SymbolKind::TypeAlias,
                signature: signature.clone(),
                signature_hash: signature_hash.clone(),
                structural_hash: sh,
                visibility: Some("pub".to_string()),
                start_line: node.start_position().row + 1,
                start_col: node.start_position().column,
                end_line: node.end_position().row + 1,
                end_col: node.end_position().column,
                children: vec![],
                parent_symbol_id: None,
                docstring: docstring.clone(),
                cyclomatic: None,
                cognitive: None,
                max_nesting: None,
                flags: 0,
                language_attrs: None,
            });
        }
    }
}

fn extract_macro(node: Node, src: &[u8], file_path: &str) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(src).ok()?.to_string();
    let flags = extract_flags(file_path, &name, node);
    let docstring = extract_docstring(node, src);
    let sh = Some(structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    ));

    Some(ExtractedSymbol {
        qualified_name: name.clone(),
        short_name: name,
        kind: SymbolKind::Macro,
        signature: None,
        signature_hash: None,
        structural_hash: sh,
        visibility: Some("pub".to_string()),
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        children: vec![],
        parent_symbol_id: None,
        docstring,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags,
        language_attrs: None,
    })
}

fn extract_const_define(node: Node, src: &[u8], file_path: &str) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(src).ok()?.to_string();

    if is_header_guard(&name) {
        return None;
    }

    let flags = extract_flags(file_path, &name, node);
    let docstring = extract_docstring(node, src);
    let sh = Some(structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    ));

    Some(ExtractedSymbol {
        qualified_name: name.clone(),
        short_name: name,
        kind: SymbolKind::Const,
        signature: None,
        signature_hash: None,
        structural_hash: sh,
        visibility: Some("pub".to_string()),
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        children: vec![],
        parent_symbol_id: None,
        docstring,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags,
        language_attrs: None,
    })
}

fn collect_var_declarators(
    node: Node,
    src: &[u8],
    file_path: &str,
    symbols: &mut Vec<ExtractedSymbol>,
) {
    let has_const = has_specifier(node, src, "const");
    let kind = if has_const {
        SymbolKind::Const
    } else {
        SymbolKind::Static
    };
    let visibility = extract_visibility(node, src);
    let docstring = extract_docstring(node, src);

    for declarator in field_children(node, "declarator") {
        if find_function_declarator(declarator).is_some() {
            continue;
        }
        if let Some(name) = find_name_in_declarator(declarator, src) {
            let name_ident = find_name_node_in_declarator(declarator);
            let sh = Some(structural_hash::compute(
                node,
                src,
                name_ident.map(|n| (n.start_byte(), n.end_byte())),
            ));
            let flags = extract_flags(file_path, &name, node);
            symbols.push(ExtractedSymbol {
                qualified_name: name.clone(),
                short_name: name,
                kind,
                signature: None,
                signature_hash: None,
                structural_hash: sh,
                visibility: visibility.clone(),
                start_line: node.start_position().row + 1,
                start_col: node.start_position().column,
                end_line: node.end_position().row + 1,
                end_col: node.end_position().column,
                children: vec![],
                parent_symbol_id: None,
                docstring: docstring.clone(),
                cyclomatic: None,
                cognitive: None,
                max_nesting: None,
                flags,
                language_attrs: None,
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Declarator navigation
// ---------------------------------------------------------------------------

fn field_children<'a>(node: Node<'a>, field: &str) -> Vec<Node<'a>> {
    let mut result = Vec::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if cursor.field_name() == Some(field) {
                result.push(cursor.node());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    result
}

fn find_name_in_declarator(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" | "type_identifier" | "field_identifier" | "primitive_type" => {
            node.utf8_text(src).ok().map(|s| s.to_string())
        }
        "pointer_declarator"
        | "array_declarator"
        | "init_declarator"
        | "parenthesized_declarator"
        | "function_declarator" => node
            .child_by_field_name("declarator")
            .and_then(|d| find_name_in_declarator(d, src)),
        _ => None,
    }
}

fn find_name_node_in_declarator(node: Node) -> Option<Node> {
    match node.kind() {
        "identifier" | "type_identifier" | "field_identifier" | "primitive_type" => Some(node),
        "pointer_declarator"
        | "array_declarator"
        | "init_declarator"
        | "parenthesized_declarator"
        | "function_declarator" => node
            .child_by_field_name("declarator")
            .and_then(find_name_node_in_declarator),
        _ => None,
    }
}

fn find_function_declarator(node: Node) -> Option<Node> {
    match node.kind() {
        "function_declarator" => Some(node),
        "pointer_declarator" | "parenthesized_declarator" => node
            .child_by_field_name("declarator")
            .and_then(find_function_declarator),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn extract_visibility(node: Node, src: &[u8]) -> Option<String> {
    Some(
        if has_specifier(node, src, "static") {
            "private"
        } else {
            "pub"
        }
        .to_string(),
    )
}

fn has_specifier(node: Node, src: &[u8], keyword: &str) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "storage_class_specifier" | "type_qualifier"
                if child.utf8_text(src).ok() == Some(keyword) =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn extract_docstring(node: Node, src: &[u8]) -> Option<String> {
    let mut doc_lines: Vec<String> = Vec::new();
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        if sib.kind() != "comment" {
            break;
        }
        if let Ok(text) = sib.utf8_text(src) {
            if let Some(inner) = text.strip_prefix("/**") {
                let inner = inner.strip_suffix("*/").unwrap_or(inner).trim();
                doc_lines.push(inner.to_string());
            } else if let Some(inner) = text.strip_prefix("/*") {
                let inner = inner.strip_suffix("*/").unwrap_or(inner).trim();
                doc_lines.push(inner.to_string());
            } else {
                let content = text
                    .strip_prefix("// ")
                    .or_else(|| text.strip_prefix("//"))
                    .unwrap_or(text);
                doc_lines.push(content.to_string());
            }
        }
        sibling = sib.prev_sibling();
    }

    if doc_lines.is_empty() {
        None
    } else {
        doc_lines.reverse();
        Some(doc_lines.join("\n"))
    }
}

fn build_fn_signature(node: Node, src: &[u8]) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let mut sig_start = node.child_by_field_name("type")?.start_byte();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.start_byte() >= sig_start {
            break;
        }
        if child.kind() == "type_qualifier" {
            sig_start = sig_start.min(child.start_byte());
        }
    }
    let sig_bytes = &src[sig_start..body.start_byte()];
    let sig = std::str::from_utf8(sig_bytes).ok()?.trim();
    Some(sig.to_string())
}

fn extract_fn_language_attrs(node: Node, src: &[u8], declarator: Node) -> Option<String> {
    let mut attrs = serde_json::Map::new();

    if has_pointer_return(declarator) {
        attrs.insert("returns_ptr".into(), true.into());
    }

    if let Some(type_node) = node.child_by_field_name("type")
        && type_node.utf8_text(src).ok() == Some("void")
        && !has_pointer_return(declarator)
    {
        attrs.insert("returns_void".into(), true.into());
    }

    if has_specifier(node, src, "static") {
        attrs.insert("is_static".into(), true.into());
    }
    if has_specifier(node, src, "inline") {
        attrs.insert("is_inline".into(), true.into());
    }
    if has_specifier(node, src, "const") {
        attrs.insert("has_const".into(), true.into());
    }

    if let Some(func_decl) = find_function_declarator(declarator)
        && let Some(params) = func_decl.child_by_field_name("parameters")
    {
        let mut pcursor = params.walk();
        for child in params.children(&mut pcursor) {
            if child.kind() == "variadic_parameter" {
                attrs.insert("is_variadic".into(), true.into());
            }
        }

        let params_text = params.utf8_text(src).unwrap_or("");
        if params_text.contains('*') {
            attrs.insert("takes_ptr".into(), true.into());
        }
        if params_text.contains("struct ") {
            attrs.insert("has_struct_param".into(), true.into());
        }
        if !attrs.contains_key("has_const") && params_text.contains("const ") {
            attrs.insert("has_const".into(), true.into());
        }
    }

    Some(serde_json::to_string(&attrs).unwrap_or_else(|_| "{}".into()))
}

fn has_pointer_return(declarator: Node) -> bool {
    if declarator.kind() == "pointer_declarator" {
        return declarator
            .child_by_field_name("declarator")
            .is_some_and(|d| find_function_declarator(d).is_some());
    }
    false
}

// ---------------------------------------------------------------------------
// Reference extraction
// ---------------------------------------------------------------------------

fn collect_references(refs: &mut Vec<ExtractedRef>, node: Node, src: &[u8]) {
    let mut cursor = node.walk();
    walk_refs_recursive(refs, &mut cursor, src);
}

fn walk_refs_recursive(refs: &mut Vec<ExtractedRef>, cursor: &mut TreeCursor, src: &[u8]) {
    let node = cursor.node();
    let kind = node.kind();

    match kind {
        "identifier" if !is_definition_name(node) => {
            if let Ok(name) = node.utf8_text(src) {
                let context_kind = classify_ref_context(node);
                if context_kind != RefContextKind::Other {
                    refs.push(ExtractedRef {
                        name: name.to_string(),
                        line: node.start_position().row + 1,
                        col: node.start_position().column,
                        context_kind,
                        resolved_local_target: None,
                        receiver: None,
                    });
                }
            }
        }
        "type_identifier" if !is_definition_name(node) => {
            if let Ok(name) = node.utf8_text(src) {
                let context_kind = classify_ref_context(node);
                if context_kind != RefContextKind::Other {
                    refs.push(ExtractedRef {
                        name: name.to_string(),
                        line: node.start_position().row + 1,
                        col: node.start_position().column,
                        context_kind,
                        resolved_local_target: None,
                        receiver: None,
                    });
                }
            }
        }
        "field_identifier" => {
            if let Some(parent) = node.parent()
                && parent.kind() == "field_expression"
                && let Ok(name) = node.utf8_text(src)
            {
                let context_kind = if parent
                    .parent()
                    .is_some_and(|gp| gp.kind() == "call_expression")
                {
                    RefContextKind::Call
                } else {
                    RefContextKind::FieldAccess
                };
                refs.push(ExtractedRef {
                    name: name.to_string(),
                    line: node.start_position().row + 1,
                    col: node.start_position().column,
                    context_kind,
                    resolved_local_target: None,
                    receiver: None,
                });
            }
        }
        _ => {}
    }

    if cursor.goto_first_child() {
        loop {
            walk_refs_recursive(refs, cursor, src);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn is_definition_name(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "function_declarator" | "init_declarator" => parent
            .child_by_field_name("declarator")
            .is_some_and(|d| d.id() == node.id()),
        "preproc_function_def"
        | "preproc_def"
        | "struct_specifier"
        | "enum_specifier"
        | "enumerator" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        "type_definition" | "parameter_declaration" | "field_declaration" | "declaration" => parent
            .child_by_field_name("declarator")
            .is_some_and(|d| d.id() == node.id()),
        "pointer_declarator" | "array_declarator" | "parenthesized_declarator" => parent
            .child_by_field_name("declarator")
            .is_some_and(|d| d.id() == node.id()),
        "labeled_statement" => parent
            .child_by_field_name("label")
            .is_some_and(|l| l.id() == node.id()),
        _ => false,
    }
}

fn classify_ref_context(node: Node) -> RefContextKind {
    let Some(parent) = node.parent() else {
        return RefContextKind::Other;
    };

    match parent.kind() {
        "call_expression" => {
            if parent
                .child_by_field_name("function")
                .is_some_and(|f| f.id() == node.id())
            {
                return RefContextKind::Call;
            }
        }
        "field_expression" if node.kind() == "field_identifier" => {
            return RefContextKind::FieldAccess;
        }
        _ => {}
    }

    if node.kind() == "type_identifier" {
        if is_compound_literal_type(node) {
            return RefContextKind::Construction;
        }
        return RefContextKind::TypeUse;
    }

    if matches!(
        parent.kind(),
        "type_descriptor" | "sized_type_specifier" | "struct_specifier" | "enum_specifier"
    ) {
        return RefContextKind::TypeUse;
    }

    RefContextKind::Other
}

fn is_compound_literal_type(node: Node) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "compound_literal_expression" => return true,
            "type_descriptor" | "struct_specifier" => {
                current = parent;
            }
            _ => return false,
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Import extraction (#include)
// ---------------------------------------------------------------------------

fn collect_includes(node: Node, src: &[u8]) -> Vec<ExtractedImport> {
    let mut imports = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "preproc_include"
            && let Some(path_node) = child.child_by_field_name("path")
            && let Ok(raw_path) = path_node.utf8_text(src)
        {
            imports.push(ExtractedImport {
                raw_path: raw_path.to_string(),
                line: child.start_position().row + 1,
                kind: "import",
                alias: None,
                is_test: false,
            });
        }
    }
    imports
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::adapter::{LanguageAdapter, ParserPool};
    use std::time::Duration;

    struct CTestAdapter;
    impl LanguageAdapter for CTestAdapter {
        fn language_id(&self) -> &str {
            "c"
        }
        fn extensions(&self) -> &[&str] {
            &["c", "h"]
        }
        fn grammar(&self) -> tree_sitter::Language {
            tree_sitter_c::LANGUAGE.into()
        }
        fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
            super::parse(ctx)
        }
    }

    fn parse_c(code: &str) -> ParseResult {
        let adapter = CTestAdapter;
        let mut pool = ParserPool::new(Duration::from_secs(5));
        pool.parse_with(&adapter, code, "test.c").unwrap()
    }

    #[test]
    fn smoke_parse_function() {
        let r = parse_c("int main(void) { return 0; }");
        assert!(r.parsed_ok);
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].short_name, "main");
        assert_eq!(r.symbols[0].kind, SymbolKind::Function);
    }

    #[test]
    fn function_signature_extracted() {
        let r = parse_c("int add(int a, int b) { return a + b; }");
        let sig = r.symbols[0].signature.as_deref().unwrap();
        assert!(sig.contains("add"));
        assert!(sig.contains("int a"));
        assert!(sig.contains("int b"));
    }

    #[test]
    fn pointer_return_detected() {
        let r = parse_c("char *get_name(void) { return 0; }");
        let attrs = r.symbols[0].language_attrs.as_deref().unwrap();
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(attrs).unwrap();
        assert_eq!(map.get("returns_ptr"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn void_return_detected() {
        let r = parse_c("void do_stuff(void) {}");
        let attrs = r.symbols[0].language_attrs.as_deref().unwrap();
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(attrs).unwrap();
        assert_eq!(
            map.get("returns_void"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn static_function_is_private() {
        let r = parse_c("static int helper(void) { return 1; }");
        assert_eq!(r.symbols[0].visibility.as_deref(), Some("private"));
        let attrs = r.symbols[0].language_attrs.as_deref().unwrap();
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(attrs).unwrap();
        assert_eq!(map.get("is_static"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn non_static_function_is_pub() {
        let r = parse_c("int foo(void) { return 0; }");
        assert_eq!(r.symbols[0].visibility.as_deref(), Some("pub"));
    }

    #[test]
    fn struct_with_body_extracted() {
        let r = parse_c("struct Point { int x; int y; };");
        let structs: Vec<_> = r
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Struct)
            .collect();
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].short_name, "Point");
    }

    #[test]
    fn enum_with_body_extracted() {
        let r = parse_c("enum Color { RED, GREEN, BLUE };");
        let enums: Vec<_> = r
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Enum)
            .collect();
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].short_name, "Color");
    }

    #[test]
    fn typedef_extracted() {
        let r = parse_c("typedef unsigned long size_t;");
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].short_name, "size_t");
        assert_eq!(r.symbols[0].kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn typedef_struct_extracts_both() {
        let r = parse_c("typedef struct Node { int val; } Node;");
        let kinds: Vec<_> = r.symbols.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&SymbolKind::Struct));
        assert!(kinds.contains(&SymbolKind::TypeAlias));
    }

    #[test]
    fn preproc_function_def_is_macro() {
        let r = parse_c("#define MAX(a, b) ((a) > (b) ? (a) : (b))");
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].short_name, "MAX");
        assert_eq!(r.symbols[0].kind, SymbolKind::Macro);
    }

    #[test]
    fn preproc_def_is_const() {
        let r = parse_c("#define BUFFER_SIZE 1024");
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].short_name, "BUFFER_SIZE");
        assert_eq!(r.symbols[0].kind, SymbolKind::Const);
    }

    #[test]
    fn header_guard_filtered() {
        let r = parse_c(
            "#define MY_HEADER_H\n#define MY_HEADER_H_\n#define MY_HEADER_INCLUDED\n#define MY_HEADER_INCLUDED_\n#define REAL_CONST 42",
        );
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].short_name, "REAL_CONST");
    }

    #[test]
    fn global_static_var() {
        let r = parse_c("static int count = 0;");
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].kind, SymbolKind::Static);
        assert_eq!(r.symbols[0].visibility.as_deref(), Some("private"));
    }

    #[test]
    fn global_const_var() {
        let r = parse_c("const int MAX = 100;");
        let consts: Vec<_> = r
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Const)
            .collect();
        assert_eq!(consts.len(), 1);
        assert_eq!(consts[0].short_name, "MAX");
    }

    #[test]
    fn docstring_block_comment() {
        let r = parse_c("/** Adds two numbers */\nint add(int a, int b) { return a + b; }");
        assert_eq!(r.symbols[0].docstring.as_deref(), Some("Adds two numbers"));
    }

    #[test]
    fn docstring_line_comments() {
        let r = parse_c("// First line\n// Second line\nint foo(void) { return 0; }");
        let doc = r.symbols[0].docstring.as_deref().unwrap();
        assert!(doc.contains("First line"));
        assert!(doc.contains("Second line"));
    }

    #[test]
    fn call_expression_ref() {
        let r = parse_c("void f(void) { printf(\"hi\"); }");
        let calls: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::Call)
            .collect();
        assert!(calls.iter().any(|c| c.name == "printf"));
    }

    #[test]
    fn field_access_ref() {
        let r = parse_c("struct S { int x; };\nvoid f(void) { struct S s; s.x = 1; }");
        let fields: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::FieldAccess)
            .collect();
        assert!(fields.iter().any(|f| f.name == "x"));
    }

    #[test]
    fn type_use_ref() {
        let r = parse_c("typedef int MyInt;\nvoid f(MyInt x) {}");
        let types: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(types.iter().any(|t| t.name == "MyInt"));
    }

    #[test]
    fn include_quotes_preserved() {
        let r = parse_c("#include <stdio.h>\n#include \"myheader.h\"\nvoid f(void) {}");
        assert_eq!(r.imports.len(), 2);
        assert!(r.imports.iter().any(|i| i.raw_path == "<stdio.h>"));
        assert!(r.imports.iter().any(|i| i.raw_path == "\"myheader.h\""));
    }

    #[test]
    fn test_file_heuristic() {
        assert!(is_test_file("foo_test.c"));
        assert!(is_test_file("test_foo.c"));
        assert!(is_test_file("tests/bar.c"));
        assert!(!is_test_file("main.c"));
    }

    #[test]
    fn test_function_flag() {
        let r = parse_c("void test_something(void) {}");
        assert_ne!(r.symbols[0].flags & FLAG_TEST, 0);
    }

    #[test]
    fn variadic_detected() {
        let r = parse_c("int my_printf(const char *fmt, ...) { return 0; }");
        let attrs = r.symbols[0].language_attrs.as_deref().unwrap();
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(attrs).unwrap();
        assert_eq!(map.get("is_variadic"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(map.get("takes_ptr"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(map.get("has_const"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn complexity_computed() {
        let r = parse_c("int f(int x) { if (x > 0) { return 1; } else { return 0; } }");
        assert!(r.symbols[0].cyclomatic.unwrap() > 1);
        assert!(r.symbols[0].cognitive.unwrap() > 0);
    }

    #[test]
    fn extern_declaration_skipped() {
        let r = parse_c("extern int global_var;");
        let vars: Vec<_> = r
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Static || s.kind == SymbolKind::Const)
            .collect();
        assert!(vars.is_empty());
    }

    #[test]
    fn inline_detected() {
        let r = parse_c("inline int fast(void) { return 1; }");
        let attrs = r.symbols[0].language_attrs.as_deref().unwrap();
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(attrs).unwrap();
        assert_eq!(map.get("is_inline"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn struct_param_detected() {
        let r = parse_c("void f(struct Point p) {}");
        let attrs = r.symbols[0].language_attrs.as_deref().unwrap();
        let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(attrs).unwrap();
        assert_eq!(
            map.get("has_struct_param"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn cyclomatic_counts_c_constructs() {
        let code = r#"
int f(int x) {
    if (x > 0) { return 1; }
    for (int i = 0; i < x; i++) {}
    while (x) { x--; }
    switch (x) {
        case 0: break;
        case 1: break;
    }
    if (x > 0 && x < 10) {}
    return 0;
}
"#;
        let r = parse_c(code);
        // base 1 + 2*if + for + while + 2*case - switch + && = 7
        assert_eq!(r.symbols[0].cyclomatic, Some(7));
    }

    #[test]
    fn cognitive_scores_nesting() {
        let code = "int f(int x) { if (x > 0) { if (x < 10) { return 1; } } return 0; }";
        let r = parse_c(code);
        // outer if: +1 (nesting 0), inner if: +1+1 (nesting 1) = 3
        assert_eq!(r.symbols[0].cognitive, Some(3));
    }

    #[test]
    fn pointer_var_not_in_refs() {
        let r = parse_c("int *ptr = 0;");
        assert!(!r.references.iter().any(|r| r.name == "ptr"));
    }

    #[test]
    fn array_var_not_in_refs() {
        let r = parse_c("int arr[10];");
        assert!(!r.references.iter().any(|r| r.name == "arr"));
    }

    #[test]
    fn docstring_on_typedef_struct() {
        let r = parse_c("/** A node */\ntypedef struct Node { int val; } Node;");
        let s = r
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Struct)
            .unwrap();
        assert!(s.docstring.as_deref().unwrap().contains("A node"));
    }

    #[test]
    fn docstring_on_declaration_struct() {
        let r = parse_c("/** A point */\nstruct Point { int x; int y; } origin;");
        let s = r
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Struct)
            .unwrap();
        assert!(s.docstring.as_deref().unwrap().contains("A point"));
    }

    #[test]
    fn comma_separated_vars() {
        let r = parse_c("static int a, b, c;");
        let statics: Vec<_> = r
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Static)
            .collect();
        assert_eq!(statics.len(), 3);
        let names: Vec<_> = statics.iter().map(|s| s.short_name.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(names.contains(&"c"));
    }

    #[test]
    fn comma_separated_typedefs() {
        let r = parse_c("typedef int Int, Integer;");
        let aliases: Vec<_> = r
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::TypeAlias)
            .collect();
        assert_eq!(aliases.len(), 2);
        let names: Vec<_> = aliases.iter().map(|s| s.short_name.as_str()).collect();
        assert!(names.contains(&"Int"));
        assert!(names.contains(&"Integer"));
    }

    #[test]
    fn signature_includes_const_qualifier() {
        let r = parse_c("const int get_val(void) { return 0; }");
        let sig = r.symbols[0].signature.as_deref().unwrap();
        assert!(sig.starts_with("const"));
    }

    #[test]
    fn ternary_counted_in_cyclomatic() {
        let r = parse_c("int f(int x) { return x > 0 ? 1 : 0; }");
        // base 1 + ternary = 2
        assert_eq!(r.symbols[0].cyclomatic, Some(2));
    }

    #[test]
    fn ternary_counted_in_cognitive() {
        let r = parse_c("int f(int x) { return x > 0 ? 1 : 0; }");
        // ternary: +1
        assert_eq!(r.symbols[0].cognitive, Some(1));
    }

    #[test]
    fn struct_fields_extracted() {
        let r = parse_c("struct Point { int x; int y; };");
        let s = r
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Struct)
            .unwrap();
        assert_eq!(s.children.len(), 2);
        assert_eq!(s.children[0].short_name, "x");
        assert_eq!(s.children[0].kind, SymbolKind::Field);
        assert_eq!(s.children[0].qualified_name, "Point::x");
        assert_eq!(s.children[0].signature.as_deref(), Some("int x"));
        assert_eq!(s.children[1].short_name, "y");
        assert_eq!(s.children[1].qualified_name, "Point::y");
    }

    #[test]
    fn struct_pointer_field_extracted() {
        let r = parse_c("struct Node { struct Node *next; int val; };");
        let s = r
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Struct)
            .unwrap();
        assert_eq!(s.children.len(), 2);
        let next = s.children.iter().find(|f| f.short_name == "next").unwrap();
        assert_eq!(next.kind, SymbolKind::Field);
        assert_eq!(next.qualified_name, "Node::next");
    }

    #[test]
    fn typedef_struct_fields_extracted() {
        let r = parse_c("typedef struct Pair { int a; int b; } Pair;");
        let s = r
            .symbols
            .iter()
            .find(|s| s.kind == SymbolKind::Struct)
            .unwrap();
        assert_eq!(s.children.len(), 2);
        assert_eq!(s.children[0].short_name, "a");
        assert_eq!(s.children[0].kind, SymbolKind::Field);
    }
}
