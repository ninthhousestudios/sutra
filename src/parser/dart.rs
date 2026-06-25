use crate::error::Result;
use crate::parser::adapter::ParseContext;
use crate::parser::rust::{FLAG_CFG_TEST, FLAG_FFI_ENTRY, FLAG_OVERRIDE, FLAG_TEST};

pub const DART_LIFECYCLE_METHODS: &[&str] = &[
    "build",
    "initState",
    "dispose",
    "didChangeDependencies",
    "didUpdateWidget",
    "deactivate",
    "reassemble",
];
use crate::parser::{
    ExtractedImport, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, SymbolKind,
    complexity,
};
use tree_sitter::{Node, TreeCursor};

pub fn parse(ctx: &ParseContext) -> Result<ParseResult> {
    let root = ctx.tree.root_node();
    let parsed_ok = !root.has_error();
    let src = ctx.source;

    let symbols = collect_symbols(root, src, &[], ctx.file_path);

    let mut references = Vec::new();
    collect_references(&mut references, root, src);

    let mut imports = Vec::new();
    collect_imports(&mut imports, root, src);

    Ok(ParseResult {
        file_path: ctx.file_path.to_string(),
        language: "dart".to_string(),
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
// Symbol extraction
// ---------------------------------------------------------------------------

fn collect_symbols(
    node: Node,
    src: &[u8],
    name_context: &[String],
    file_path: &str,
) -> Vec<ExtractedSymbol> {
    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "class_declaration" => {
                if let Some(mut sym) =
                    extract_named_symbol(child, src, name_context, SymbolKind::Class)
                {
                    sym.language_attrs =
                        extract_language_attrs(child, None, src, SymbolKind::Class);
                    sym.flags |= extract_flags(child, src, file_path, None);
                    let name = sym.short_name.clone();
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(name);
                        sym.children = collect_symbols(body, src, &ctx, file_path);
                    }
                    symbols.push(sym);
                }
                continue;
            }
            "mixin_declaration" => {
                if let Some(mut sym) =
                    extract_named_symbol(child, src, name_context, SymbolKind::Mixin)
                {
                    sym.language_attrs =
                        extract_language_attrs(child, None, src, SymbolKind::Mixin);
                    sym.flags |= extract_flags(child, src, file_path, None);
                    let name = sym.short_name.clone();
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(name);
                        sym.children = collect_symbols(body, src, &ctx, file_path);
                    }
                    symbols.push(sym);
                }
                continue;
            }
            "extension_declaration" => {
                if let Some(mut sym) =
                    extract_named_symbol(child, src, name_context, SymbolKind::Extension)
                {
                    sym.flags |= extract_flags(child, src, file_path, None);
                    let name = sym.short_name.clone();
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(name);
                        sym.children = collect_symbols(body, src, &ctx, file_path);
                    }
                    symbols.push(sym);
                }
                continue;
            }
            "enum_declaration" => {
                if let Some(mut sym) =
                    extract_named_symbol(child, src, name_context, SymbolKind::Enum)
                {
                    sym.flags |= extract_flags(child, src, file_path, None);
                    symbols.push(sym);
                }
            }
            "function_declaration" => {
                let sig_node = child.child_by_field_name("signature").unwrap_or(child);
                let kind = if name_context.is_empty() {
                    SymbolKind::Function
                } else {
                    SymbolKind::Method
                };
                if let Some(mut sym) = extract_fn_symbol(sig_node, child, src, name_context, kind) {
                    sym.language_attrs = extract_language_attrs(child, Some(sig_node), src, kind);
                    sym.flags |= extract_flags(child, src, file_path, Some(&sym.short_name));
                    symbols.push(sym);
                }
            }
            "method_declaration" | "getter_declaration" | "setter_declaration" => {
                let sig_node = child.child_by_field_name("signature").unwrap_or(child);
                let kind = if name_context.is_empty() {
                    SymbolKind::Function
                } else {
                    SymbolKind::Method
                };
                if let Some(mut sym) =
                    extract_method_symbol(sig_node, child, src, name_context, kind)
                {
                    sym.language_attrs = extract_language_attrs(child, Some(sig_node), src, kind);
                    sym.flags |= extract_flags(child, src, file_path, Some(&sym.short_name));
                    symbols.push(sym);
                }
            }
            "top_level_variable_declaration" => {
                symbols.extend(extract_variable_symbols(
                    child,
                    src,
                    name_context,
                    file_path,
                ));
            }
            // External declarations in class bodies: `declaration` wraps
            // [external?, getter_signature | setter_signature | function_signature]
            // or field declarations (static_final_declaration_list / initialized_identifier_list)
            "declaration" => {
                let mut c = child.walk();
                if let Some(sig) = child
                    .children(&mut c)
                    .find(|n| is_dart_signature_node(n.kind()))
                {
                    let kind = if name_context.is_empty() {
                        SymbolKind::Function
                    } else {
                        SymbolKind::Method
                    };
                    if let Some(mut sym) =
                        extract_method_symbol(sig, child, src, name_context, kind)
                    {
                        sym.language_attrs = extract_language_attrs(child, Some(sig), src, kind);
                        sym.flags |= extract_flags(child, src, file_path, Some(&sym.short_name));
                        symbols.push(sym);
                    }
                } else {
                    symbols.extend(extract_variable_symbols(
                        child,
                        src,
                        name_context,
                        file_path,
                    ));
                }
            }
            "constructor_signature" => {
                if let Some(mut sym) =
                    extract_method_symbol(child, child, src, name_context, SymbolKind::Method)
                {
                    sym.language_attrs =
                        extract_language_attrs(child, Some(child), src, SymbolKind::Method);
                    sym.flags |= extract_flags(child, src, file_path, Some(&sym.short_name));
                    symbols.push(sym);
                }
            }
            "type_alias" => {
                if let Some(mut sym) = extract_type_alias(child, src, name_context) {
                    sym.flags |= extract_flags(child, src, file_path, None);
                    symbols.push(sym);
                }
            }
            _ => {
                symbols.extend(collect_symbols(child, src, name_context, file_path));
            }
        }
    }
    symbols
}

/// Extract a symbol where the name is in the `name` field of `node`.
fn extract_named_symbol(
    node: Node,
    src: &[u8],
    name_context: &[String],
    kind: SymbolKind,
) -> Option<ExtractedSymbol> {
    let short_name = node
        .child_by_field_name("name")?
        .utf8_text(src)
        .ok()?
        .to_string();

    build_symbol(node, src, name_context, short_name, kind, None, None)
}

/// Extract a function symbol. `sig_node` carries name/params/return_type fields;
/// `span_node` is used for position (the outer function_declaration).
fn extract_fn_symbol(
    sig_node: Node,
    span_node: Node,
    src: &[u8],
    name_context: &[String],
    kind: SymbolKind,
) -> Option<ExtractedSymbol> {
    let short_name = sig_node
        .child_by_field_name("name")?
        .utf8_text(src)
        .ok()?
        .to_string();
    let (signature, signature_hash) = build_fn_signature(sig_node, src, &short_name);
    build_symbol(
        span_node,
        src,
        name_context,
        short_name,
        kind,
        signature,
        signature_hash,
    )
}

/// Extract a method symbol from a method_signature node (which may wrap function_signature,
/// getter_signature, or setter_signature).
fn extract_method_symbol(
    sig_node: Node,
    span_node: Node,
    src: &[u8],
    name_context: &[String],
    kind: SymbolKind,
) -> Option<ExtractedSymbol> {
    // method_signature wraps one of: function_signature, getter_signature, setter_signature,
    // factory_constructor_signature, or constructor_signature.
    // It may also have leading keyword children like "static" — skip those.
    let inner = if sig_node.kind() == "method_signature" {
        let mut c = sig_node.walk();
        sig_node
            .children(&mut c)
            .find(|n| is_dart_signature_node(n.kind()))
            .unwrap_or(sig_node)
    } else {
        sig_node
    };

    let short_name = inner
        .child_by_field_name("name")?
        .utf8_text(src)
        .ok()?
        .to_string();
    let (signature, signature_hash) = build_fn_signature(inner, src, &short_name);
    build_symbol(
        span_node,
        src,
        name_context,
        short_name,
        kind,
        signature,
        signature_hash,
    )
}

/// Extract a type_alias symbol. The grammar gives: (type_alias (type_identifier) ...).
/// There is no `name` field — the name is the first `type_identifier` child.
fn extract_type_alias(node: Node, src: &[u8], name_context: &[String]) -> Option<ExtractedSymbol> {
    let mut cursor = node.walk();
    let short_name = node
        .children(&mut cursor)
        .find(|c| c.kind() == "type_identifier")?
        .utf8_text(src)
        .ok()?
        .to_string();
    build_symbol(
        node,
        src,
        name_context,
        short_name,
        SymbolKind::TypeAlias,
        None,
        None,
    )
}

fn extract_variable_symbols(
    node: Node,
    src: &[u8],
    name_context: &[String],
    file_path: &str,
) -> Vec<ExtractedSymbol> {
    let mut symbols = Vec::new();

    let has_keyword = |kw: &str| -> bool {
        let mut cursor = node.walk();
        node.children(&mut cursor).any(|c| c.kind() == kw)
    };

    let kind = if has_keyword("const") || has_keyword("final") {
        SymbolKind::Const
    } else {
        SymbolKind::Static
    };

    let type_text = {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|c| c.kind() == "type")
            .and_then(|t| t.utf8_text(src).ok())
            .map(|s| s.to_string())
    };

    let modifier = if has_keyword("const") {
        Some("const")
    } else if has_keyword("final") {
        Some("final")
    } else if has_keyword("var") {
        Some("var")
    } else {
        None
    };

    let build_sig = |name: &str| -> Option<String> {
        let mut parts = Vec::new();
        if let Some(m) = modifier {
            parts.push(m.to_string());
        }
        if let Some(ref t) = type_text {
            parts.push(t.clone());
        }
        parts.push(name.to_string());
        Some(parts.join(" "))
    };

    let mut cursor = node.walk();
    for list_child in node.children(&mut cursor) {
        match list_child.kind() {
            "static_final_declaration_list" => {
                let mut lc = list_child.walk();
                for decl in list_child.children(&mut lc) {
                    if decl.kind() != "static_final_declaration" {
                        continue;
                    }
                    let mut dc = decl.walk();
                    if let Some(ident) = decl.children(&mut dc).find(|c| c.kind() == "identifier")
                        && let Ok(name) = ident.utf8_text(src)
                    {
                        let sig = build_sig(name);
                        if let Some(mut sym) =
                            build_symbol(decl, src, name_context, name.to_string(), kind, sig, None)
                        {
                            sym.language_attrs = extract_language_attrs(node, None, src, kind);
                            sym.flags |= extract_flags(node, src, file_path, None);
                            symbols.push(sym);
                        }
                    }
                }
            }
            "initialized_identifier_list" => {
                let mut lc = list_child.walk();
                for decl in list_child.children(&mut lc) {
                    if decl.kind() != "initialized_identifier" {
                        continue;
                    }
                    let mut dc = decl.walk();
                    if let Some(ident) = decl.children(&mut dc).find(|c| c.kind() == "identifier")
                        && let Ok(name) = ident.utf8_text(src)
                    {
                        let sig = build_sig(name);
                        if let Some(mut sym) =
                            build_symbol(decl, src, name_context, name.to_string(), kind, sig, None)
                        {
                            sym.language_attrs = extract_language_attrs(node, None, src, kind);
                            sym.flags |= extract_flags(node, src, file_path, None);
                            symbols.push(sym);
                        }
                    }
                }
            }
            "identifier_list" => {
                let mut lc = list_child.walk();
                for child in list_child.children(&mut lc) {
                    if child.kind() != "identifier" {
                        continue;
                    }
                    if let Ok(name) = child.utf8_text(src) {
                        let sig = build_sig(name);
                        if let Some(mut sym) = build_symbol(
                            child,
                            src,
                            name_context,
                            name.to_string(),
                            kind,
                            sig,
                            None,
                        ) {
                            sym.language_attrs = extract_language_attrs(node, None, src, kind);
                            sym.flags |= extract_flags(node, src, file_path, None);
                            symbols.push(sym);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    symbols
}

fn extract_language_attrs(
    node: Node,
    sig_node: Option<Node>,
    src: &[u8],
    kind: SymbolKind,
) -> Option<String> {
    let mut attrs = serde_json::Map::new();

    let has_keyword = |n: Node, kw: &str| -> bool {
        let mut cursor = n.walk();
        n.children(&mut cursor).any(|c| c.kind() == kw)
    };

    match kind {
        SymbolKind::Class => {
            if has_keyword(node, "abstract") {
                attrs.insert("is_abstract".into(), true.into());
            }
            if has_keyword(node, "sealed") {
                attrs.insert("is_sealed".into(), true.into());
            }
            if has_keyword(node, "base") {
                attrs.insert("is_base".into(), true.into());
            }
            if has_keyword(node, "interface") {
                attrs.insert("is_interface".into(), true.into());
            }
        }
        SymbolKind::Mixin if has_keyword(node, "base") => {
            attrs.insert("is_base".into(), true.into());
        }
        SymbolKind::Function | SymbolKind::Method => {
            if let Some(sig) = sig_node {
                let sig_inner = if sig.kind() == "method_signature" {
                    let mut c = sig.walk();
                    sig.children(&mut c)
                        .find(|n| is_dart_signature_node(n.kind()))
                        .unwrap_or(sig)
                } else {
                    sig
                };

                if sig_inner.kind() == "factory_constructor_signature" {
                    attrs.insert("is_factory".into(), true.into());
                }
                if sig_inner.kind() == "constructor_signature" {
                    attrs.insert("is_constructor".into(), true.into());
                }
                if sig_inner.kind() == "getter_signature" {
                    attrs.insert("is_getter".into(), true.into());
                }
                if sig_inner.kind() == "setter_signature" {
                    attrs.insert("is_setter".into(), true.into());
                }

                if has_keyword(sig, "static") || has_keyword(node, "static") {
                    attrs.insert("is_static".into(), true.into());
                }

                if let Some(ret_type) = sig_inner
                    .child_by_field_name("return_type")
                    .or_else(|| sig_inner.child_by_field_name("type"))
                    && let Ok(type_text) = ret_type.utf8_text(src)
                    && (type_text.starts_with("Future") || type_text.starts_with("FutureOr"))
                {
                    attrs.insert("returns_future".into(), true.into());
                }
            }

            if let Some(body) = node.child_by_field_name("body")
                && (has_keyword(body, "async") || has_keyword(body, "async*"))
            {
                attrs.insert("is_async".into(), true.into());
            }
        }
        SymbolKind::Const | SymbolKind::Static => {
            if has_keyword(node, "const") {
                attrs.insert("is_const".into(), true.into());
            }
            if has_keyword(node, "final") {
                attrs.insert("is_final".into(), true.into());
            }
            if has_keyword(node, "static") {
                attrs.insert("is_static".into(), true.into());
            }
            if has_keyword(node, "late") {
                attrs.insert("is_late".into(), true.into());
            }
        }
        _ => {}
    }

    if has_annotation(node, src, "override") {
        attrs.insert("has_override".into(), true.into());
    }

    Some(serde_json::to_string(&attrs).unwrap_or_else(|_| "{}".into()))
}

fn has_annotation(node: Node, src: &[u8], name: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| {
        c.kind() == "annotation"
            && c.child_by_field_name("name")
                .and_then(|n| n.utf8_text(src).ok())
                .is_some_and(|text| text == name)
    })
}

fn extract_flags(node: Node, src: &[u8], file_path: &str, short_name: Option<&str>) -> u32 {
    let mut flags = 0u32;

    if has_annotation(node, src, "isTest")
        || has_annotation(node, src, "isTestGroup")
        || has_annotation(node, src, "Test")
    {
        flags |= FLAG_TEST;
    }

    if file_path.ends_with("_test.dart") || file_path.starts_with("test/") {
        flags |= FLAG_CFG_TEST;
    }

    if has_annotation(node, src, "override") {
        flags |= FLAG_OVERRIDE;
        if short_name.is_some_and(|n| DART_LIFECYCLE_METHODS.contains(&n)) {
            flags |= FLAG_FFI_ENTRY;
        }
    }

    let mut anno_cursor = node.walk();
    for child in node.children(&mut anno_cursor) {
        if child.kind() == "annotation"
            && let Ok(text) = child.utf8_text(src)
            && text.contains("vm:entry-point")
        {
            flags |= FLAG_FFI_ENTRY;
        }
    }

    flags
}

fn build_symbol(
    node: Node,
    src: &[u8],
    name_context: &[String],
    short_name: String,
    kind: SymbolKind,
    signature: Option<String>,
    signature_hash: Option<String>,
) -> Option<ExtractedSymbol> {
    let qualified_name = build_qualified_name(name_context, &short_name);

    let visibility = dart_visibility(&short_name);
    let docstring = extract_docstring(node, src);

    let (cyclomatic, cognitive, max_nesting) =
        if matches!(kind, SymbolKind::Function | SymbolKind::Method) {
            if let Some(body) = node.child_by_field_name("body") {
                (
                    Some(complexity::cyclomatic(body, src, "dart")),
                    Some(complexity::cognitive(body, src, "dart")),
                    Some(complexity::max_nesting_depth(body, src, "dart")),
                )
            } else {
                (Some(1), Some(0), Some(0))
            }
        } else {
            (None, None, None)
        };

    Some(ExtractedSymbol {
        qualified_name,
        short_name,
        kind,
        signature,
        signature_hash,
        visibility,
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        children: vec![],
        docstring,
        cyclomatic,
        cognitive,
        max_nesting,
        flags: 0,
        language_attrs: Some("{}".into()),
    })
}

fn build_qualified_name(context: &[String], name: &str) -> String {
    if context.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", context.join("::"), name)
    }
}

fn is_dart_signature_node(kind: &str) -> bool {
    matches!(
        kind,
        "function_signature"
            | "getter_signature"
            | "setter_signature"
            | "factory_constructor_signature"
            | "constructor_signature"
    )
}

/// Dart visibility: names starting with `_` are private, everything else is public.
fn dart_visibility(name: &str) -> Option<String> {
    if name.starts_with('_') {
        Some("private".to_string())
    } else {
        Some("public".to_string())
    }
}

fn extract_docstring(node: Node, src: &[u8]) -> Option<String> {
    let mut doc_lines: Vec<String> = Vec::new();

    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        if let Ok(text) = sib.utf8_text(src)
            && sib.kind() == "comment"
            && text.starts_with("///")
        {
            let content = text
                .strip_prefix("/// ")
                .or_else(|| text.strip_prefix("///"))
                .unwrap_or(text);
            doc_lines.push(content.to_string());
            sibling = sib.prev_sibling();
            continue;
        }
        break;
    }

    if doc_lines.is_empty() {
        None
    } else {
        doc_lines.reverse();
        Some(doc_lines.join("\n"))
    }
}

/// Build a function/method signature string. Returns (signature, blake3_hash).
fn build_fn_signature(node: Node, src: &[u8], name: &str) -> (Option<String>, Option<String>) {
    let params_text = node
        .child_by_field_name("parameters")
        .and_then(|n| n.utf8_text(src).ok())
        .unwrap_or("()");

    let ret_text = node
        .child_by_field_name("return_type")
        .and_then(|n| n.utf8_text(src).ok());

    let sig = if let Some(ret) = ret_text {
        format!("{} {}{}", ret.trim(), name, params_text)
    } else {
        format!("{}{}", name, params_text)
    };

    let hash = blake3::hash(sig.as_bytes()).to_hex().to_string();
    (Some(sig), Some(hash))
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

    if (node.kind() == "identifier" || node.kind() == "type_identifier")
        && !is_definition_name(node)
        && let Ok(name) = node.utf8_text(src)
    {
        refs.push(ExtractedRef {
            name: name.to_string(),
            line: node.start_position().row + 1,
            col: node.start_position().column,
            context_kind: classify_ref_context(node),
        });
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
    if let Some(parent) = node.parent() {
        let is_def = matches!(
            parent.kind(),
            "class_declaration"
                | "mixin_declaration"
                | "extension_declaration"
                | "enum_declaration"
                | "function_signature"
                | "getter_signature"
                | "setter_signature"
        );
        if is_def && let Some(name_node) = parent.child_by_field_name("name") {
            return name_node.id() == node.id();
        }
    }
    false
}

fn classify_ref_context(node: Node) -> RefContextKind {
    if let Some(parent) = node.parent() {
        match parent.kind() {
            "call_expression" => return RefContextKind::Call,
            "library_import" | "import_specification" | "import_or_export" => {
                return RefContextKind::Import;
            }
            "member_expression" => return RefContextKind::FieldAccess,
            "type_name" | "type_identifier" => return RefContextKind::TypeUse,
            _ => {}
        }
    }
    RefContextKind::Other
}

// ---------------------------------------------------------------------------
// Import extraction
// ---------------------------------------------------------------------------

fn collect_imports(imports: &mut Vec<ExtractedImport>, node: Node, src: &[u8]) {
    let mut cursor = node.walk();
    walk_imports_recursive(imports, &mut cursor, src);
}

fn walk_imports_recursive(imports: &mut Vec<ExtractedImport>, cursor: &mut TreeCursor, src: &[u8]) {
    let node = cursor.node();

    if node.kind() == "import_or_export" {
        extract_import_uri(node, src, imports);
        return;
    }

    if cursor.goto_first_child() {
        loop {
            walk_imports_recursive(imports, cursor, src);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// Walk down import_or_export → library_import → import_specification → uri → string_literal
/// to extract the raw URI string.
fn extract_import_uri(node: Node, src: &[u8], imports: &mut Vec<ExtractedImport>) {
    let line = node.start_position().row + 1;
    find_string_literal(node, src)
        .into_iter()
        .for_each(|raw_path| {
            imports.push(ExtractedImport {
                raw_path,
                line,
                kind: "import",
            })
        });
}

fn find_string_literal(node: Node, src: &[u8]) -> Vec<String> {
    if node.kind() == "string_literal"
        && let Ok(text) = node.utf8_text(src)
    {
        let raw = text.trim_matches(|c| c == '\'' || c == '"').to_string();
        return vec![raw];
    }
    // Recurse into children
    let mut result = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        result.extend(find_string_literal(child, src));
        if !result.is_empty() {
            break; // one import path per import_or_export
        }
    }
    result
}
