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
    complexity, structural_hash,
};
use tree_sitter::{Node, TreeCursor};

pub fn parse(ctx: &ParseContext) -> Result<ParseResult> {
    let root = ctx.tree.root_node();
    let parsed_ok = !root.has_error();
    let src = ctx.source;

    let symbols = collect_symbols(root, src, &[], ctx.file_path);

    let mut references = Vec::new();
    collect_references(&mut references, root, src);

    // Phase B: constructor type tracking — annotate method-call refs whose
    // receiver has a known type with a type-tracking hint so the resolver can
    // prefer the class member over an unrelated global of the same name.
    let mut type_bindings: Vec<DartTypeBinding> = Vec::new();
    collect_dart_type_bindings(root, src, &mut type_bindings);
    for r in &mut references {
        if r.context_kind == RefContextKind::Call
            && let Some(recv) = &r.receiver
            && let Some(class_name) = lookup_receiver_type(&type_bindings, r.line, recv)
        {
            r.resolved_local_target = Some(format!("{}{}", TYPE_TRACKING_PREFIX, class_name));
        }
    }

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
    let name_node = node.child_by_field_name("name")?;
    let short_name = name_node.utf8_text(src).ok()?.to_string();
    let sh = Some(structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    ));

    build_symbol(node, src, name_context, short_name, kind, None, None, sh)
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
    let name_node = sig_node.child_by_field_name("name")?;
    let short_name = name_node.utf8_text(src).ok()?.to_string();
    let (signature, signature_hash) = build_fn_signature(sig_node, src, &short_name);
    let sh = Some(structural_hash::compute(
        span_node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    ));
    build_symbol(
        span_node,
        src,
        name_context,
        short_name,
        kind,
        signature,
        signature_hash,
        sh,
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

    let name_node = inner.child_by_field_name("name")?;
    let short_name = name_node.utf8_text(src).ok()?.to_string();
    let (signature, signature_hash) = build_fn_signature(inner, src, &short_name);
    let sh = Some(structural_hash::compute(
        span_node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    ));
    build_symbol(
        span_node,
        src,
        name_context,
        short_name,
        kind,
        signature,
        signature_hash,
        sh,
    )
}

/// Extract a type_alias symbol. The grammar gives: (type_alias (type_identifier) ...).
/// There is no `name` field — the name is the first `type_identifier` child.
fn extract_type_alias(node: Node, src: &[u8], name_context: &[String]) -> Option<ExtractedSymbol> {
    let mut cursor = node.walk();
    let name_child = node
        .children(&mut cursor)
        .find(|c| c.kind() == "type_identifier")?;
    let short_name = name_child.utf8_text(src).ok()?.to_string();
    let sh = Some(structural_hash::compute(
        node,
        src,
        Some((name_child.start_byte(), name_child.end_byte())),
    ));
    build_symbol(
        node,
        src,
        name_context,
        short_name,
        SymbolKind::TypeAlias,
        None,
        None,
        sh,
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

    let is_instance_field = !name_context.is_empty() && !has_keyword("static");
    let kind = if is_instance_field {
        SymbolKind::Field
    } else if has_keyword("const") || has_keyword("final") {
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
                        let sh = Some(structural_hash::compute(
                            decl,
                            src,
                            Some((ident.start_byte(), ident.end_byte())),
                        ));
                        if let Some(mut sym) = build_symbol(
                            decl,
                            src,
                            name_context,
                            name.to_string(),
                            kind,
                            sig,
                            None,
                            sh,
                        ) {
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
                        let sh = Some(structural_hash::compute(
                            decl,
                            src,
                            Some((ident.start_byte(), ident.end_byte())),
                        ));
                        if let Some(mut sym) = build_symbol(
                            decl,
                            src,
                            name_context,
                            name.to_string(),
                            kind,
                            sig,
                            None,
                            sh,
                        ) {
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
                        let sh = Some(structural_hash::compute(child, src, None));
                        if let Some(mut sym) = build_symbol(
                            child,
                            src,
                            name_context,
                            name.to_string(),
                            kind,
                            sig,
                            None,
                            sh,
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
        SymbolKind::Const | SymbolKind::Static | SymbolKind::Field => {
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

/// Whether `path` is Dart test code by convention. Dart has no attribute
/// equivalent to `#[cfg(test)]` — a package's tests live under `test/` (or
/// `integration_test/`) and are named `*_test.dart` — so path is the only
/// signal test scope has (sutra/292).
pub fn is_test_path(path: &str) -> bool {
    path.ends_with("_test.dart")
        || crate::parser::adapter::path_in_test_dir(path)
        || crate::parser::adapter::path_has_dir_segment(path, "integration_test")
}

fn extract_flags(node: Node, src: &[u8], file_path: &str, short_name: Option<&str>) -> u32 {
    let mut flags = 0u32;

    if has_annotation(node, src, "isTest")
        || has_annotation(node, src, "isTestGroup")
        || has_annotation(node, src, "Test")
    {
        flags |= FLAG_TEST;
    }

    if is_test_path(file_path) {
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

#[allow(clippy::too_many_arguments)]
fn build_symbol(
    node: Node,
    src: &[u8],
    name_context: &[String],
    short_name: String,
    kind: SymbolKind,
    signature: Option<String>,
    signature_hash: Option<String>,
    structural_hash: Option<String>,
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
        structural_hash,
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
        let context_kind = classify_ref_context(node);
        if context_kind != RefContextKind::Other {
            let receiver = if context_kind == RefContextKind::Call {
                extract_call_receiver(node, src)
            } else {
                None
            };
            refs.push(ExtractedRef {
                name: name.to_string(),
                line: node.start_position().row + 1,
                col: node.start_position().column,
                context_kind,
                resolved_local_target: None,
                receiver,
            });
        } else if node.kind() == "identifier"
            && name.starts_with('_')
            && !is_dart_binding_name(node)
        {
            // A bare value read of a private (underscore-prefixed) name — a
            // const/variable read or a method/function tear-off. Private names
            // are file-local by Dart semantics, so intra-file resolution counts
            // these as inbound references and stops the symbol reading as dead
            // (sutra/288). Definition names are already filtered above by
            // `is_definition_name`; `is_dart_binding_name` excludes the
            // remaining declaration positions (variable/field/parameter names)
            // so a symbol is never treated as referencing itself.
            refs.push(ExtractedRef {
                name: name.to_string(),
                line: node.start_position().row + 1,
                col: node.start_position().column,
                context_kind: RefContextKind::Read,
                resolved_local_target: None,
                receiver: None,
            });
        }
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

/// True when `node` is the *name* being introduced by a Dart binding
/// (variable / field / const / parameter declaration), as opposed to a value
/// referenced on its right-hand side. Definition names for classes, mixins,
/// functions and accessors are handled separately by `is_definition_name`;
/// this covers the declaration forms it does not, so a `Read` ref is never
/// emitted for a symbol's own declaration site (which would make it reference
/// itself and mask genuine dead code).
fn is_dart_binding_name(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        // The whole subtree is a binding target, never a value read.
        // `identifier_list` holds the names of a bare (uninitialized) field or
        // variable declaration — `external static var _a, _b;` — so every
        // identifier child is a declaration name. `catch_clause` exposes only
        // its `exception`/`stack_trace` bindings as direct identifier children
        // (the handler body is a nested block).
        "formal_parameter"
        | "type_parameter"
        | "label"
        | "declared_identifier"
        | "constructor_signature"
        | "identifier_list"
        | "catch_clause" => true,
        // `name = value` forms: the declared name is the first identifier and
        // sits before the initializer; a value read appears after it.
        "static_final_declaration"
        | "initialized_identifier"
        | "initialized_variable_definition" => {
            first_identifier_child(parent).map(|n| n.id()) == Some(node.id())
        }
        // for-in: `for (var _x in _items)` — the `name` field is the loop
        // binding, but the `value` field (the iterable) is a genuine read that
        // must still emit a ref, so guard on the field rather than the parent.
        "for_statement" => parent.child_by_field_name("name").map(|n| n.id()) == Some(node.id()),
        _ => false,
    }
}

/// First direct child of `node` that is an `identifier`, if any.
fn first_identifier_child(node: Node) -> Option<Node> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|c| c.kind() == "identifier")
}

fn classify_ref_context(node: Node) -> RefContextKind {
    let Some(parent) = node.parent() else {
        return RefContextKind::Other;
    };
    let pk = parent.kind();

    // Construction: type_identifier inside new/const expressions.
    // new Foo()  → new_expression > type > type_identifier
    // const Foo() → const_object_expression > type > type_identifier
    if node.kind() == "type_identifier" && is_construction_name(node) {
        return RefContextKind::Construction;
    }

    // Call: direct callee of call_expression
    if pk == "call_expression" {
        return RefContextKind::Call;
    }

    // Call: property side of member_expression inside call_expression.
    // http.get() → call_expression > member_expression > identifier("get")
    // Only the property child is the callee; the object (http) falls through.
    if pk == "member_expression"
        && is_property_child(node, parent)
        && parent
            .parent()
            .is_some_and(|gp| gp.kind() == "call_expression")
    {
        return RefContextKind::Call;
    }

    // Cascade call: direct child of cascade_call_expression (..bar())
    if pk == "cascade_call_expression" {
        return RefContextKind::Call;
    }

    // Cascade chained call: property side of cascade_member_expression
    // inside cascade_call_expression (..bar().baz())
    if pk == "cascade_member_expression"
        && is_property_child(node, parent)
        && parent
            .parent()
            .is_some_and(|gp| gp.kind() == "cascade_call_expression")
    {
        return RefContextKind::Call;
    }

    // Cascade field: identifier inside cascade_selector (..x)
    if pk == "cascade_selector" {
        return RefContextKind::FieldAccess;
    }

    // Cascade field access: property side of cascade_member_expression
    // not caught by the call check above (..foo().bar where bar is not called)
    if pk == "cascade_member_expression" && is_property_child(node, parent) {
        return RefContextKind::FieldAccess;
    }

    // Import
    if matches!(
        pk,
        "library_import" | "import_specification" | "import_or_export"
    ) {
        return RefContextKind::Import;
    }

    // Type use: any ancestor is a `type` node (covers generics, nullable,
    // return types, parameter types, extends/implements/on/with clauses)
    if has_type_ancestor(node) {
        return RefContextKind::TypeUse;
    }

    // Field access: property side of member_expression (not already caught as Call above)
    if pk == "member_expression" && is_property_child(node, parent) {
        return RefContextKind::FieldAccess;
    }

    RefContextKind::Other
}

/// If `node` is the property/callee of a member_expression call (e.g. `get` in
/// `c.get()`), return the receiver identifier text (e.g. `"c"`).
fn extract_call_receiver(node: Node, src: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    if parent.kind() == "member_expression" && is_property_child(node, parent) {
        let obj = parent.child_by_field_name("object")?;
        if obj.kind() == "identifier" || obj.kind() == "this_expression" {
            return obj.utf8_text(src).ok().map(|s| s.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Dart type-tracking scope — maps local variables to their constructor type
// ---------------------------------------------------------------------------

/// A type binding recorded when we see `final/var/Type x = ClassName(...)`.
/// `scope_end_line` bounds the binding to its enclosing function so that
/// bindings from sibling functions don't leak across scope boundaries.
struct DartTypeBinding {
    var_name: String,
    class_name: String,
    decl_line: usize,
    scope_end_line: usize,
}

/// Walk the entire file AST and collect constructor-type bindings from every
/// `initialized_identifier` or `initialized_variable_definition` whose initializer is a
/// constructor call or `new`/`const` expression.
fn collect_dart_type_bindings(node: Node, src: &[u8], bindings: &mut Vec<DartTypeBinding>) {
    if (node.kind() == "initialized_identifier" || node.kind() == "initialized_variable_definition")
        && let Some(b) = extract_dart_type_binding(node, src)
    {
        bindings.push(b);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_dart_type_bindings(child, src, bindings);
    }
}

fn extract_dart_type_binding(node: Node, src: &[u8]) -> Option<DartTypeBinding> {
    let name_node = node.child_by_field_name("name")?;
    let value_node = node.child_by_field_name("value")?;

    let var_name = name_node.utf8_text(src).ok()?.to_string();
    let decl_line = name_node.start_position().row + 1;
    let class_name = dart_constructor_type(value_node, src)?;
    let scope_end_line = enclosing_function_end(node);

    Some(DartTypeBinding {
        var_name,
        class_name,
        decl_line,
        scope_end_line,
    })
}

fn enclosing_function_end(node: Node) -> usize {
    const FUNC_KINDS: &[&str] = &[
        "function_body",
        "method_declaration",
        "function_declaration",
        "constructor_declaration",
    ];
    let mut cur = node;
    while let Some(parent) = cur.parent() {
        if FUNC_KINDS.contains(&parent.kind()) {
            return parent.end_position().row + 1;
        }
        cur = parent;
    }
    usize::MAX
}

/// If `node` is a constructor-call expression, return the class name.
/// Handles:
///   `Cache()` → call_expression with identifier "Cache" (uppercase)
///   `Cache.fromJson()` → call_expression with member_expression, object "Cache"
///   `new Cache()` → new_expression
///   `const Cache()` → const_object_expression
fn dart_constructor_type(node: Node, src: &[u8]) -> Option<String> {
    match node.kind() {
        "call_expression" => {
            let func = node.child_by_field_name("function")?;
            match func.kind() {
                "identifier" | "type_identifier" => {
                    let name = func.utf8_text(src).ok()?;
                    if name.chars().next().is_some_and(|c| c.is_uppercase()) {
                        return Some(name.to_string());
                    }
                }
                "member_expression" => {
                    let obj = func.child_by_field_name("object")?;
                    let obj_name = obj.utf8_text(src).ok()?;
                    if obj_name.chars().next().is_some_and(|c| c.is_uppercase()) {
                        return Some(obj_name.to_string());
                    }
                }
                _ => {}
            }
            None
        }
        "new_expression" | "const_object_expression" => {
            let type_node = node.child_by_field_name("type")?;
            // type node wraps the class name; walk into it for the identifier
            find_first_type_name(type_node, src)
        }
        _ => None,
    }
}

fn find_first_type_name(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() == "type_identifier" || node.kind() == "identifier" {
        return node.utf8_text(src).ok().map(|s| s.to_string());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(n) = find_first_type_name(child, src) {
            return Some(n);
        }
    }
    None
}

/// Look up the most recently declared type for `receiver_name` before `ref_line`,
/// constrained to bindings whose enclosing function scope contains `ref_line`.
fn lookup_receiver_type<'a>(
    bindings: &'a [DartTypeBinding],
    ref_line: usize,
    receiver_name: &str,
) -> Option<&'a str> {
    bindings
        .iter()
        .filter(|b| {
            b.var_name == receiver_name && b.decl_line < ref_line && ref_line <= b.scope_end_line
        })
        .max_by_key(|b| b.decl_line)
        .map(|b| b.class_name.as_str())
}

pub const TYPE_TRACKING_PREFIX: &str = "::type_tracking::";

fn is_property_child(node: Node, parent: Node) -> bool {
    parent
        .child_by_field_name("property")
        .is_some_and(|p| p.id() == node.id())
}

/// Walk ancestors to check if a type_identifier is the name of a new/const expression.
/// Handles: new Foo(), const Foo(), new Foo.named(), const Foo<T>().
fn is_construction_name(node: Node) -> bool {
    let mut current = node;
    while let Some(p) = current.parent() {
        match p.kind() {
            "new_expression" | "const_object_expression" => return true,
            "type" | "type_arguments" => current = p,
            _ => return false,
        }
    }
    false
}

/// Walk ancestors looking for a `type` node, which in tree-sitter-dart wraps
/// all type contexts: annotations, generics, nullable, extends/implements/on/with.
fn has_type_ancestor(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "type" {
            return true;
        }
        current = n.parent();
    }
    false
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
                alias: None,
                is_test: false,
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
