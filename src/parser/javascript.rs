use crate::error::Result;
use crate::parser::adapter::ParseContext;
use crate::parser::{
    ExtractedImport, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, SymbolKind,
    complexity, structural_hash,
};
use tree_sitter::{Node, TreeCursor};

pub(super) const FLAG_TEST: u32 = 0x01;

pub fn parse(ctx: &ParseContext) -> Result<ParseResult> {
    let root = ctx.tree.root_node();
    let parsed_ok = !root.has_error();
    let src = ctx.source;
    let file_path = ctx.file_path;

    let symbols = collect_symbols(root, src, file_path);

    let mut references = Vec::new();
    collect_references(&mut references, root, src);

    let mut imports = Vec::new();
    collect_imports(&mut imports, root, src);

    Ok(ParseResult {
        file_path: file_path.to_string(),
        language: "javascript".to_string(),
        symbols,
        references,
        imports,
        parsed_ok,
        line_count: std::str::from_utf8(src)
            .map(|s| s.lines().count())
            .unwrap_or(0),
    })
}

pub(super) fn collect_symbols(root: Node, src: &[u8], file_path: &str) -> Vec<ExtractedSymbol> {
    collect_symbols_inner(root, src, file_path, &[])
}

pub(super) fn collect_symbols_inner(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
) -> Vec<ExtractedSymbol> {
    let mut symbols = Vec::new();
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                if let Some(mut sym) = extract_function(child, src, file_path, name_context) {
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx: Vec<&str> = name_context.to_vec();
                        ctx.push(&sym.short_name);
                        sym.children = collect_symbols_inner(body, src, file_path, &ctx);
                    }
                    symbols.push(sym);
                }
            }
            "class_declaration" => {
                if let Some(sym) = extract_class(child, src, file_path, name_context) {
                    symbols.push(sym);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                extract_variable_declarators(
                    child,
                    src,
                    file_path,
                    name_context,
                    None,
                    &mut symbols,
                );
            }
            "export_statement" => {
                handle_export(child, src, file_path, name_context, &mut symbols);
            }
            _ => {
                symbols.extend(collect_symbols_inner(child, src, file_path, name_context));
            }
        }
    }
    symbols
}

// ---------------------------------------------------------------------------
// Function extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_function(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, src);
    let qualified_name = build_qualified_name(name_context, &name);

    let is_async = has_keyword(node, src, "async");
    let is_generator = node.kind() == "generator_function_declaration";

    let mut attrs = serde_json::Map::new();
    if is_async {
        attrs.insert("async".into(), true.into());
    }
    if is_generator {
        attrs.insert("generator".into(), true.into());
    }

    let body = node.child_by_field_name("body");
    let (cyc, cog, nesting) = complexity_triple(body, src);

    let sig = build_fn_signature(node, src, &name, is_async, is_generator);
    let sig_hash = sig
        .as_ref()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );
    let docstring = extract_jsdoc(node, src);
    let flags = extract_flags(node, src, file_path);

    Some(ExtractedSymbol {
        qualified_name,
        short_name: name,
        kind: SymbolKind::Function,
        signature: sig,
        signature_hash: sig_hash,
        structural_hash: Some(struct_hash),
        visibility: None,
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        children: Vec::new(),
        parent_symbol_id: None,
        docstring,
        cyclomatic: cyc,
        cognitive: cog,
        max_nesting: nesting,
        flags,
        language_attrs: attrs_to_json(&attrs),
    })
}

// ---------------------------------------------------------------------------
// Class extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_class(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, src);
    let qualified_name = build_qualified_name(name_context, &name);

    let docstring = extract_jsdoc(node, src);
    let flags = extract_flags(node, src, file_path);
    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );

    let mut children = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut ctx: Vec<&str> = name_context.to_vec();
        ctx.push(&name);
        extract_class_body(body, src, file_path, &ctx, &mut children);
    }

    Some(ExtractedSymbol {
        qualified_name,
        short_name: name,
        kind: SymbolKind::Class,
        signature: None,
        signature_hash: None,
        structural_hash: Some(struct_hash),
        visibility: None,
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
        flags,
        language_attrs: None,
    })
}

pub(super) fn extract_class_body(
    body: Node,
    src: &[u8],
    file_path: &str,
    ctx: &[&str],
    children: &mut Vec<ExtractedSymbol>,
) {
    for child in body.children(&mut body.walk()) {
        match child.kind() {
            "method_definition" => {
                if let Some(mut method) = extract_method(child, src, file_path, ctx) {
                    if let Some(method_body) = child.child_by_field_name("body") {
                        let mut mctx: Vec<&str> = ctx.to_vec();
                        mctx.push(&method.short_name);
                        method.children = collect_symbols_inner(method_body, src, file_path, &mctx);
                    }
                    children.push(method);
                }
            }
            "field_definition" => {
                if let Some(field) = extract_field(child, src, file_path, ctx) {
                    children.push(field);
                }
            }
            _ => {
                children.extend(collect_symbols_inner(child, src, file_path, ctx));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Method extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_method(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, src);
    let qualified_name = build_qualified_name(name_context, &name);

    let is_async = has_keyword(node, src, "async");
    let is_static = has_keyword(node, src, "static");
    let is_getter = has_keyword(node, src, "get");
    let is_setter = has_keyword(node, src, "set");
    let is_generator = node
        .children(&mut node.walk())
        .any(|c| !c.is_named() && c.utf8_text(src) == Ok("*"));
    let is_computed = name_node.kind() == "computed_property_name";

    let mut attrs = serde_json::Map::new();
    if is_async {
        attrs.insert("async".into(), true.into());
    }
    if is_static {
        attrs.insert("static".into(), true.into());
    }
    if is_getter {
        attrs.insert("getter".into(), true.into());
    }
    if is_setter {
        attrs.insert("setter".into(), true.into());
    }
    if is_generator {
        attrs.insert("generator".into(), true.into());
    }
    if is_computed {
        attrs.insert("computed".into(), true.into());
    }

    let body = node.child_by_field_name("body");
    let (cyc, cog, nesting) = complexity_triple(body, src);

    let sig = build_method_signature(
        node,
        src,
        &name,
        is_async,
        is_generator,
        is_getter,
        is_setter,
        is_static,
    );
    let sig_hash = sig
        .as_ref()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );
    let docstring = extract_jsdoc(node, src);
    let flags = extract_flags(node, src, file_path);

    Some(ExtractedSymbol {
        qualified_name,
        short_name: name,
        kind: SymbolKind::Method,
        signature: sig,
        signature_hash: sig_hash,
        structural_hash: Some(struct_hash),
        visibility: None,
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        children: Vec::new(),
        parent_symbol_id: None,
        docstring,
        cyclomatic: cyc,
        cognitive: cog,
        max_nesting: nesting,
        flags,
        language_attrs: attrs_to_json(&attrs),
    })
}

// ---------------------------------------------------------------------------
// Field extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_field(
    node: Node,
    src: &[u8],
    _file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("property")?;
    let name = node_text(name_node, src);
    let qualified_name = build_qualified_name(name_context, &name);

    let is_static = has_keyword(node, src, "static");
    let is_computed = name_node.kind() == "computed_property_name";

    let mut attrs = serde_json::Map::new();
    if is_static {
        attrs.insert("static".into(), true.into());
    }
    if is_computed {
        attrs.insert("computed".into(), true.into());
    }

    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );
    let docstring = extract_jsdoc(node, src);

    Some(ExtractedSymbol {
        qualified_name,
        short_name: name,
        kind: SymbolKind::Field,
        signature: None,
        signature_hash: None,
        structural_hash: Some(struct_hash),
        visibility: None,
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        children: Vec::new(),
        parent_symbol_id: None,
        docstring,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: attrs_to_json(&attrs),
    })
}

// ---------------------------------------------------------------------------
// Variable / arrow-function extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_variable_declarators(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
    export_vis: Option<&str>,
    symbols: &mut Vec<ExtractedSymbol>,
) {
    let decl_kind = match node.kind() {
        "lexical_declaration" => node
            .children(&mut node.walk())
            .find(|c| !c.is_named())
            .map(|c| node_text(c, src))
            .unwrap_or_default(),
        "variable_declaration" => "var".to_string(),
        _ => return,
    };

    for child in node.children(&mut node.walk()) {
        if child.kind() != "variable_declarator" {
            continue;
        }

        let name_node = match child.child_by_field_name("name") {
            Some(n) if n.kind() == "identifier" => n,
            _ => continue,
        };
        let name = node_text(name_node, src);
        let qualified_name = build_qualified_name(name_context, &name);

        let value = child.child_by_field_name("value");
        let is_arrow = value.is_some_and(|v| v.kind() == "arrow_function");
        let is_fn_expr =
            value.is_some_and(|v| v.kind() == "function" || v.kind() == "generator_function");

        let (kind, cyc, cog, nesting, sig, sig_hash, lang_attrs) = if is_arrow {
            let arrow = value.unwrap();
            let is_async = has_keyword(arrow, src, "async");
            let mut a = serde_json::Map::new();
            a.insert("arrow".into(), true.into());
            if is_async {
                a.insert("async".into(), true.into());
            }
            let body = arrow.child_by_field_name("body");
            let (c, co, n) = complexity_triple(body, src);
            let s = build_arrow_signature(arrow, src, &name, is_async);
            let sh = s
                .as_ref()
                .map(|sig| blake3::hash(sig.as_bytes()).to_hex().to_string());
            (SymbolKind::Function, c, co, n, s, sh, attrs_to_json(&a))
        } else if is_fn_expr {
            let fn_node = value.unwrap();
            let is_async = has_keyword(fn_node, src, "async");
            let is_gen = fn_node.kind() == "generator_function";
            let mut a = serde_json::Map::new();
            if is_async {
                a.insert("async".into(), true.into());
            }
            if is_gen {
                a.insert("generator".into(), true.into());
            }
            let body = fn_node.child_by_field_name("body");
            let (c, co, n) = complexity_triple(body, src);
            let s = build_fn_signature(fn_node, src, &name, is_async, is_gen);
            let sh = s
                .as_ref()
                .map(|sig| blake3::hash(sig.as_bytes()).to_hex().to_string());
            (SymbolKind::Function, c, co, n, s, sh, attrs_to_json(&a))
        } else {
            let mut a = serde_json::Map::new();
            a.insert(
                "declaration_kind".into(),
                serde_json::Value::String(decl_kind.as_str().into()),
            );
            let kind = if decl_kind == "const" {
                SymbolKind::Const
            } else {
                SymbolKind::Static
            };
            (kind, None, None, None, None, None, attrs_to_json(&a))
        };

        let struct_hash = structural_hash::compute(
            child,
            src,
            Some((name_node.start_byte(), name_node.end_byte())),
        );
        let docstring = extract_jsdoc(node, src);
        let flags = extract_flags(child, src, file_path);

        let mut sym = ExtractedSymbol {
            qualified_name,
            short_name: name,
            kind,
            signature: sig,
            signature_hash: sig_hash,
            structural_hash: Some(struct_hash),
            visibility: export_vis.map(String::from),
            start_line: child.start_position().row + 1,
            start_col: child.start_position().column,
            end_line: child.end_position().row + 1,
            end_col: child.end_position().column,
            children: Vec::new(),
            parent_symbol_id: None,
            docstring,
            cyclomatic: cyc,
            cognitive: cog,
            max_nesting: nesting,
            flags,
            language_attrs: lang_attrs,
        };

        if is_arrow || is_fn_expr {
            if let Some(v) = value {
                if let Some(body) = v.child_by_field_name("body") {
                    let mut ctx: Vec<&str> = name_context.to_vec();
                    ctx.push(&sym.short_name);
                    sym.children = collect_symbols_inner(body, src, file_path, &ctx);
                }
            }
        }

        symbols.push(sym);
    }
}

// ---------------------------------------------------------------------------
// Export handling
// ---------------------------------------------------------------------------

pub(super) fn handle_export(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
    symbols: &mut Vec<ExtractedSymbol>,
) {
    let is_default = node
        .children(&mut node.walk())
        .any(|c| !c.is_named() && c.utf8_text(src) == Ok("default"));
    let vis = if is_default {
        "export default"
    } else {
        "export"
    };

    for child in node.named_children(&mut node.walk()) {
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                if let Some(mut sym) = extract_function(child, src, file_path, name_context) {
                    sym.visibility = Some(vis.to_string());
                    if sym.docstring.is_none() {
                        sym.docstring = extract_jsdoc(node, src);
                    }
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx: Vec<&str> = name_context.to_vec();
                        ctx.push(&sym.short_name);
                        sym.children = collect_symbols_inner(body, src, file_path, &ctx);
                    }
                    symbols.push(sym);
                }
            }
            "class_declaration" => {
                if let Some(mut sym) = extract_class(child, src, file_path, name_context) {
                    sym.visibility = Some(vis.to_string());
                    if sym.docstring.is_none() {
                        sym.docstring = extract_jsdoc(node, src);
                    }
                    symbols.push(sym);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                extract_variable_declarators(
                    child,
                    src,
                    file_path,
                    name_context,
                    Some(vis),
                    symbols,
                );
                if let Some(last) = symbols.last_mut() {
                    if last.docstring.is_none() {
                        last.docstring = extract_jsdoc(node, src);
                    }
                }
            }
            "export_clause" => {
                if node.child_by_field_name("source").is_some() {
                    return;
                }
                for spec in child.named_children(&mut child.walk()) {
                    if spec.kind() != "export_specifier" {
                        continue;
                    }
                    let local_name = spec
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(src).ok());
                    if let Some(name) = local_name {
                        for sym in symbols.iter_mut().rev() {
                            if sym.short_name == name {
                                sym.visibility = Some("export".to_string());
                                break;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub(super) fn node_text(node: Node, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

pub(super) fn build_qualified_name(name_context: &[&str], name: &str) -> String {
    if name_context.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", name_context.join("::"), name)
    }
}

pub(super) fn has_keyword(node: Node, src: &[u8], keyword: &str) -> bool {
    node.children(&mut node.walk())
        .any(|c| !c.is_named() && c.utf8_text(src) == Ok(keyword))
}

pub(super) fn complexity_triple(
    body: Option<Node>,
    src: &[u8],
) -> (Option<u32>, Option<u32>, Option<u32>) {
    match body {
        Some(b) => (
            Some(complexity::cyclomatic(b, src, "javascript")),
            Some(complexity::cognitive(b, src, "javascript")),
            Some(complexity::max_nesting_depth(b, src, "javascript")),
        ),
        None => (Some(1), Some(0), Some(0)),
    }
}

pub(super) fn attrs_to_json(attrs: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    if attrs.is_empty() {
        None
    } else {
        Some(serde_json::to_string(attrs).unwrap_or_else(|_| "{}".into()))
    }
}

// ---------------------------------------------------------------------------
// JSDoc extraction
// ---------------------------------------------------------------------------

pub(super) fn extract_jsdoc(node: Node, src: &[u8]) -> Option<String> {
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        if p.kind() != ";" && p.kind() != "empty_statement" {
            break;
        }
        prev = p.prev_sibling();
    }
    let comment = prev?;
    if comment.kind() != "comment" {
        return None;
    }
    let text = comment.utf8_text(src).ok()?;
    if !text.starts_with("/**") {
        return None;
    }
    Some(strip_jsdoc(text))
}

pub(super) fn strip_jsdoc(text: &str) -> String {
    let text = text.strip_prefix("/**").unwrap_or(text);
    let text = text.strip_suffix("*/").unwrap_or(text);
    text.lines()
        .map(|line| {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("* ") {
                rest
            } else if trimmed == "*" {
                ""
            } else {
                trimmed
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Flags / test detection
// ---------------------------------------------------------------------------

pub(super) fn extract_flags(node: Node, src: &[u8], file_path: &str) -> u32 {
    let mut flags = 0u32;
    if is_test_file(file_path) {
        flags |= FLAG_TEST;
    }
    if flags == 0 && is_inside_test_call(node, src) {
        flags |= FLAG_TEST;
    }
    flags
}

pub(super) fn is_test_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".test.js")
        || lower.ends_with(".test.jsx")
        || lower.ends_with(".test.mjs")
        || lower.ends_with(".test.cjs")
        || lower.ends_with(".spec.js")
        || lower.ends_with(".spec.jsx")
        || lower.ends_with(".spec.mjs")
        || lower.ends_with(".spec.cjs")
        || lower.contains("__tests__/")
        || lower.contains("__tests__\\")
}

pub(super) fn is_inside_test_call(node: Node, src: &[u8]) -> bool {
    let mut current = node.parent();
    while let Some(p) = current {
        if p.kind() == "call_expression" {
            if let Some(func) = p.child_by_field_name("function") {
                let name = func.utf8_text(src).unwrap_or("");
                if matches!(
                    name,
                    "describe"
                        | "it"
                        | "test"
                        | "beforeEach"
                        | "afterEach"
                        | "beforeAll"
                        | "afterAll"
                ) {
                    return true;
                }
            }
        }
        current = p.parent();
    }
    false
}

// ---------------------------------------------------------------------------
// Signature building
// ---------------------------------------------------------------------------

pub(super) fn build_fn_signature(
    node: Node,
    src: &[u8],
    name: &str,
    is_async: bool,
    is_generator: bool,
) -> Option<String> {
    let params = node.child_by_field_name("parameters")?;
    let params_text = params.utf8_text(src).ok()?;
    let mut sig = String::new();
    if is_async {
        sig.push_str("async ");
    }
    sig.push_str("function");
    if is_generator {
        sig.push('*');
    }
    sig.push(' ');
    sig.push_str(name);
    sig.push_str(params_text);
    Some(sig)
}

pub(super) fn build_arrow_signature(
    node: Node,
    src: &[u8],
    name: &str,
    is_async: bool,
) -> Option<String> {
    let params_text = if let Some(params) = node.child_by_field_name("parameters") {
        params.utf8_text(src).ok()?.to_string()
    } else if let Some(param) = node.child_by_field_name("parameter") {
        format!("({})", param.utf8_text(src).ok()?)
    } else {
        return None;
    };
    let mut sig = String::new();
    if is_async {
        sig.push_str("async ");
    }
    sig.push_str("const ");
    sig.push_str(name);
    sig.push_str(" = ");
    sig.push_str(&params_text);
    sig.push_str(" =>");
    Some(sig)
}

fn build_method_signature(
    node: Node,
    src: &[u8],
    name: &str,
    is_async: bool,
    is_generator: bool,
    is_getter: bool,
    is_setter: bool,
    is_static: bool,
) -> Option<String> {
    let params = node.child_by_field_name("parameters")?;
    let params_text = params.utf8_text(src).ok()?;
    let mut sig = String::new();
    if is_static {
        sig.push_str("static ");
    }
    if is_async {
        sig.push_str("async ");
    }
    if is_getter {
        sig.push_str("get ");
    }
    if is_setter {
        sig.push_str("set ");
    }
    if is_generator {
        sig.push('*');
    }
    sig.push_str(name);
    sig.push_str(params_text);
    Some(sig)
}

// ---------------------------------------------------------------------------
// Reference extraction
// ---------------------------------------------------------------------------

pub(super) fn collect_references(refs: &mut Vec<ExtractedRef>, node: Node, src: &[u8]) {
    let mut cursor = node.walk();
    walk_refs_recursive(refs, &mut cursor, src);
}

fn walk_refs_recursive(refs: &mut Vec<ExtractedRef>, cursor: &mut TreeCursor, src: &[u8]) {
    let node = cursor.node();

    match node.kind() {
        "identifier" => {
            if !is_definition_name(node)
                && let Ok(name) = node.utf8_text(src)
            {
                let ctx = classify_ref_context(node);
                if ctx != RefContextKind::Other {
                    refs.push(ExtractedRef {
                        name: name.to_string(),
                        line: node.start_position().row + 1,
                        col: node.start_position().column,
                        context_kind: ctx,
                        resolved_local_target: None,
                        receiver: None,
                    });
                }
            }
        }
        "property_identifier" => {
            if let Some(parent) = node.parent()
                && parent.kind() == "member_expression"
                && let Ok(name) = node.utf8_text(src)
            {
                let is_call = parent
                    .parent()
                    .is_some_and(|gp| gp.kind() == "call_expression");
                let receiver = parent
                    .child_by_field_name("object")
                    .filter(|o| o.kind() == "identifier")
                    .and_then(|o| o.utf8_text(src).ok())
                    .map(|s| s.to_string());

                refs.push(ExtractedRef {
                    name: name.to_string(),
                    line: node.start_position().row + 1,
                    col: node.start_position().column,
                    context_kind: if is_call {
                        RefContextKind::Call
                    } else {
                        RefContextKind::FieldAccess
                    },
                    resolved_local_target: None,
                    receiver,
                });
            }
        }
        "shorthand_property_identifier" => {
            // `{ foo }` in an object literal — a value reference.
            if let Ok(name) = node.utf8_text(src) {
                refs.push(ExtractedRef {
                    name: name.to_string(),
                    line: node.start_position().row + 1,
                    col: node.start_position().column,
                    context_kind: RefContextKind::Other,
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
        "function_declaration" | "generator_function_declaration" | "class_declaration" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        "variable_declarator" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        "method_definition" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        "formal_parameters" => true,
        "required_parameter" | "optional_parameter" => parent
            .child_by_field_name("pattern")
            .is_some_and(|n| n.id() == node.id()),
        "assignment_pattern" => parent
            .child_by_field_name("left")
            .is_some_and(|n| n.id() == node.id()),
        "for_in_statement" | "for_statement" => parent
            .child_by_field_name("left")
            .is_some_and(|n| n.id() == node.id()),
        "catch_clause" => parent
            .child_by_field_name("parameter")
            .is_some_and(|n| n.id() == node.id()),
        "import_specifier" => parent
            .child_by_field_name("alias")
            .or_else(|| parent.child_by_field_name("name"))
            .is_some_and(|n| n.id() == node.id()),
        "import_clause" | "namespace_import" => true,
        "shorthand_property_identifier_pattern" => true,
        "pair_pattern" => parent
            .child_by_field_name("value")
            .is_some_and(|n| n.id() == node.id()),
        "arrow_function" => parent
            .child_by_field_name("parameter")
            .is_some_and(|n| n.id() == node.id()),
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
        "new_expression" => {
            if parent
                .child_by_field_name("constructor")
                .is_some_and(|c| c.id() == node.id())
            {
                return RefContextKind::Construction;
            }
        }
        "jsx_self_closing_element" | "jsx_opening_element" | "jsx_closing_element" => {
            if parent
                .child_by_field_name("name")
                .is_some_and(|n| n.id() == node.id())
            {
                return RefContextKind::Construction;
            }
        }
        _ => {}
    }

    if matches!(
        parent.kind(),
        "import_specifier" | "namespace_import" | "import_clause"
    ) {
        return RefContextKind::Import;
    }

    RefContextKind::Other
}

// ---------------------------------------------------------------------------
// Import extraction
// ---------------------------------------------------------------------------

pub(super) fn collect_imports(imports: &mut Vec<ExtractedImport>, node: Node, src: &[u8]) {
    let mut cursor = node.walk();
    walk_imports_recursive(imports, &mut cursor, src);
}

fn walk_imports_recursive(imports: &mut Vec<ExtractedImport>, cursor: &mut TreeCursor, src: &[u8]) {
    let node = cursor.node();

    match node.kind() {
        "import_statement" => {
            extract_es_import(node, src, imports);
            return;
        }
        "export_statement" => {
            if let Some(source) = node.child_by_field_name("source")
                && let Some(path) = extract_string_content(source, src)
            {
                imports.push(ExtractedImport {
                    raw_path: path,
                    line: node.start_position().row + 1,
                    kind: "re_export",
                    alias: None,
                });
            }
        }
        "call_expression" => {
            extract_require_or_dynamic_import(node, src, imports);
        }
        _ => {}
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

fn extract_es_import(node: Node, src: &[u8], imports: &mut Vec<ExtractedImport>) {
    let Some(source) = node.child_by_field_name("source") else {
        return;
    };
    let Some(raw_path) = extract_string_content(source, src) else {
        return;
    };
    let line = node.start_position().row + 1;

    let mut alias = None;
    let mut cursor = node.walk();
    for clause in node
        .children(&mut cursor)
        .filter(|c| c.kind() == "import_clause")
    {
        let mut cc = clause.walk();
        for child in clause.children(&mut cc) {
            match child.kind() {
                "namespace_import" => {
                    if let Some(name) = child.child_by_field_name("name").or_else(|| {
                        let mut nc = child.walk();
                        child.children(&mut nc).find(|n| n.kind() == "identifier")
                    }) {
                        alias = name.utf8_text(src).ok().map(|s| s.to_string());
                    }
                }
                "identifier" => {
                    alias = child.utf8_text(src).ok().map(|s| s.to_string());
                }
                _ => {}
            }
        }
    }

    imports.push(ExtractedImport {
        raw_path,
        line,
        kind: "es_import",
        alias,
    });
}

fn extract_require_or_dynamic_import(node: Node, src: &[u8], imports: &mut Vec<ExtractedImport>) {
    let Some(func) = node.child_by_field_name("function") else {
        return;
    };
    let Ok(func_name) = func.utf8_text(src) else {
        return;
    };

    let kind = match func_name {
        "require" => "require",
        "import" => "dynamic_import",
        _ => return,
    };

    let Some(args) = node.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = args.walk();
    for child in args.children(&mut cursor) {
        if (child.kind() == "string" || child.kind() == "template_string")
            && let Some(path) = extract_string_content(child, src)
        {
            imports.push(ExtractedImport {
                raw_path: path,
                line: node.start_position().row + 1,
                kind,
                alias: None,
            });
            return;
        }
    }
}

fn extract_string_content(node: Node, src: &[u8]) -> Option<String> {
    let text = node.utf8_text(src).ok()?;
    let trimmed = text.trim_matches(|c| c == '\'' || c == '"' || c == '`');
    Some(trimmed.to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::adapter::ParserPool;
    use std::time::Duration;

    struct Adapter;
    impl crate::parser::adapter::LanguageAdapter for Adapter {
        fn language_id(&self) -> &str {
            "javascript"
        }
        fn extensions(&self) -> &[&str] {
            &["js"]
        }
        fn grammar(&self) -> tree_sitter::Language {
            tree_sitter_javascript::LANGUAGE.into()
        }
        fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
            super::parse(ctx)
        }
    }

    fn parse_js(code: &str) -> ParseResult {
        let mut pool = ParserPool::new(Duration::from_secs(5));
        pool.parse_with(&Adapter, code, "test.js").unwrap()
    }

    fn parse_js_file(code: &str, path: &str) -> ParseResult {
        let mut pool = ParserPool::new(Duration::from_secs(5));
        pool.parse_with(&Adapter, code, path).unwrap()
    }

    #[test]
    fn function_declaration() {
        let result = parse_js("function foo(a, b) { return a + b; }");
        assert_eq!(result.symbols.len(), 1);
        let sym = &result.symbols[0];
        assert_eq!(sym.short_name, "foo");
        assert_eq!(sym.kind, SymbolKind::Function);
        assert!(
            sym.signature
                .as_ref()
                .unwrap()
                .contains("function foo(a, b)")
        );
        assert!(sym.structural_hash.is_some());
    }

    #[test]
    fn async_function() {
        let result = parse_js("async function fetchData(url) { }");
        let sym = &result.symbols[0];
        assert_eq!(sym.short_name, "fetchData");
        assert_eq!(sym.kind, SymbolKind::Function);
        let attrs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(sym.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(attrs.get("async"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn generator_function() {
        let result = parse_js("function* gen() { yield 1; }");
        let sym = &result.symbols[0];
        assert_eq!(sym.short_name, "gen");
        assert_eq!(sym.kind, SymbolKind::Function);
        let attrs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(sym.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(attrs.get("generator"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn arrow_function_const() {
        let result = parse_js("const add = (a, b) => a + b;");
        assert_eq!(result.symbols.len(), 1);
        let sym = &result.symbols[0];
        assert_eq!(sym.short_name, "add");
        assert_eq!(sym.kind, SymbolKind::Function);
        let attrs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(sym.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(attrs.get("arrow"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn class_with_methods_and_fields() {
        let result = parse_js(
            r#"
class Animal {
    species = 'unknown';

    constructor(name) {
        this.name = name;
    }
    speak() {
        return this.name;
    }
    get info() {
        return this.species;
    }
    set info(value) {
        this.species = value;
    }
    static create(name) {
        return new Animal(name);
    }
}
"#,
        );
        assert_eq!(result.symbols.len(), 1);
        let class = &result.symbols[0];
        assert_eq!(class.short_name, "Animal");
        assert_eq!(class.kind, SymbolKind::Class);

        let names: Vec<&str> = class
            .children
            .iter()
            .map(|c| c.short_name.as_str())
            .collect();
        assert!(names.contains(&"species"));
        assert!(names.contains(&"constructor"));
        assert!(names.contains(&"speak"));
        assert!(names.contains(&"info"));
        assert!(names.contains(&"create"));

        let constructor = class
            .children
            .iter()
            .find(|c| c.short_name == "constructor")
            .unwrap();
        assert_eq!(constructor.qualified_name, "Animal::constructor");
        assert_eq!(constructor.kind, SymbolKind::Method);

        let field = class
            .children
            .iter()
            .find(|c| c.short_name == "species")
            .unwrap();
        assert_eq!(field.kind, SymbolKind::Field);
    }

    #[test]
    fn variable_declarations() {
        let result = parse_js("const MAX = 100;\nlet count = 0;\nvar name = 'hello';");
        assert_eq!(result.symbols.len(), 3);

        let max = result
            .symbols
            .iter()
            .find(|s| s.short_name == "MAX")
            .unwrap();
        assert_eq!(max.kind, SymbolKind::Const);
        let attrs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(max.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(
            attrs.get("declaration_kind").unwrap().as_str(),
            Some("const")
        );

        let count = result
            .symbols
            .iter()
            .find(|s| s.short_name == "count")
            .unwrap();
        assert_eq!(count.kind, SymbolKind::Static);
    }

    #[test]
    fn export_visibility() {
        let result = parse_js("export function greet() {}\nexport default function main() {}");
        let greet = result
            .symbols
            .iter()
            .find(|s| s.short_name == "greet")
            .unwrap();
        assert_eq!(greet.visibility, Some("export".to_string()));

        let main = result
            .symbols
            .iter()
            .find(|s| s.short_name == "main")
            .unwrap();
        assert_eq!(main.visibility, Some("export default".to_string()));
    }

    #[test]
    fn nested_functions() {
        let result = parse_js(
            r#"
function outer() {
    function inner() {
        return 42;
    }
    return inner();
}
"#,
        );
        assert_eq!(result.symbols.len(), 1);
        let outer = &result.symbols[0];
        assert_eq!(outer.short_name, "outer");
        assert_eq!(outer.children.len(), 1);
        let inner = &outer.children[0];
        assert_eq!(inner.short_name, "inner");
        assert_eq!(inner.qualified_name, "outer::inner");
    }

    #[test]
    fn jsdoc_extraction() {
        let result = parse_js(
            r#"
/**
 * Adds two numbers.
 * @param {number} a
 * @param {number} b
 * @returns {number}
 */
function add(a, b) { return a + b; }
"#,
        );
        let sym = &result.symbols[0];
        assert!(sym.docstring.is_some());
        assert!(sym.docstring.as_ref().unwrap().contains("Adds two numbers"));
    }

    #[test]
    fn test_flag_from_test_file() {
        let result = parse_js_file("function foo() {}", "app.test.js");
        assert_ne!(result.symbols[0].flags & FLAG_TEST, 0);
    }

    #[test]
    fn test_flag_inside_describe() {
        let result = parse_js(
            r#"
describe("math", function() {
    function helper() { return 1; }
});
"#,
        );
        let helpers: Vec<_> = result
            .symbols
            .iter()
            .filter(|s| s.short_name == "helper")
            .collect();
        assert_eq!(helpers.len(), 1);
        assert_ne!(helpers[0].flags & FLAG_TEST, 0);
    }

    #[test]
    fn export_const_arrow() {
        let result = parse_js("export const handler = async (req, res) => {};");
        assert_eq!(result.symbols.len(), 1);
        let sym = &result.symbols[0];
        assert_eq!(sym.short_name, "handler");
        assert_eq!(sym.kind, SymbolKind::Function);
        assert_eq!(sym.visibility, Some("export".to_string()));
        let attrs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(sym.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(attrs.get("arrow"), Some(&serde_json::Value::Bool(true)));
        assert_eq!(attrs.get("async"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn named_export_clause() {
        let result = parse_js(
            r#"
function foo() { return 1; }
const bar = 42;
export { foo, bar };
"#,
        );
        let foo = result
            .symbols
            .iter()
            .find(|s| s.short_name == "foo")
            .unwrap();
        assert_eq!(foo.visibility, Some("export".to_string()));
        let bar = result
            .symbols
            .iter()
            .find(|s| s.short_name == "bar")
            .unwrap();
        assert_eq!(bar.visibility, Some("export".to_string()));
    }

    #[test]
    fn named_export_does_not_affect_reexports() {
        let result = parse_js(r#"export { x } from './other';"#);
        assert!(result.symbols.is_empty());
    }

    #[test]
    fn jsdoc_on_exported_function() {
        let result = parse_js(
            r#"
/** Greets the user. */
export function greet(name) { return `hi ${name}`; }
"#,
        );
        let sym = result
            .symbols
            .iter()
            .find(|s| s.short_name == "greet")
            .unwrap();
        assert_eq!(sym.visibility, Some("export".to_string()));
        assert!(sym.docstring.is_some());
        assert!(sym.docstring.as_ref().unwrap().contains("Greets the user"));
    }

    #[test]
    fn jsdoc_on_exported_const_arrow() {
        let result = parse_js(
            r#"
/** Handles requests. */
export const handler = async (req) => {};
"#,
        );
        let sym = result
            .symbols
            .iter()
            .find(|s| s.short_name == "handler")
            .unwrap();
        assert_eq!(sym.visibility, Some("export".to_string()));
        assert!(sym.docstring.is_some());
        assert!(sym.docstring.as_ref().unwrap().contains("Handles requests"));
    }

    #[test]
    fn jsdoc_on_exported_class() {
        let result = parse_js(
            r#"
/** A widget. */
export class Widget {}
"#,
        );
        let sym = result
            .symbols
            .iter()
            .find(|s| s.short_name == "Widget")
            .unwrap();
        assert_eq!(sym.visibility, Some("export".to_string()));
        assert!(sym.docstring.is_some());
        assert!(sym.docstring.as_ref().unwrap().contains("A widget"));
    }

    #[test]
    fn static_class_field() {
        let result = parse_js(
            r#"
class Config {
    static DEFAULT_TIMEOUT = 3000;
}
"#,
        );
        let class = &result.symbols[0];
        let field = class
            .children
            .iter()
            .find(|c| c.short_name == "DEFAULT_TIMEOUT")
            .unwrap();
        assert_eq!(field.kind, SymbolKind::Field);
        let attrs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(field.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(attrs.get("static"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn function_call_refs() {
        let result = parse_js("function foo() {} function bar() { foo(); }");
        let call_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::Call)
            .collect();
        assert!(
            call_refs.iter().any(|r| r.name == "foo"),
            "should find call ref to foo"
        );
        assert!(
            !result.references.iter().any(|r| r.name == "bar"),
            "bar is a def, not a ref"
        );
    }

    #[test]
    fn method_call_refs() {
        let result = parse_js("const x = obj.method();");
        let call_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::Call)
            .collect();
        assert!(
            call_refs
                .iter()
                .any(|r| r.name == "method" && r.receiver.as_deref() == Some("obj"))
        );
    }

    #[test]
    fn member_access_refs() {
        let result = parse_js("const x = obj.field;");
        let field_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::FieldAccess)
            .collect();
        assert!(field_refs.iter().any(|r| r.name == "field"));
    }

    #[test]
    fn constructor_refs() {
        let result = parse_js("const x = new Foo();");
        let ctor_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::Construction)
            .collect();
        assert!(ctor_refs.iter().any(|r| r.name == "Foo"));
    }

    #[test]
    fn es_import_extraction() {
        let result = parse_js("import { foo, bar } from 'module-name';");
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].raw_path, "module-name");
        assert_eq!(result.imports[0].kind, "es_import");
    }

    #[test]
    fn es_default_import() {
        let result = parse_js("import foo from 'module-name';");
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].raw_path, "module-name");
        assert_eq!(result.imports[0].alias.as_deref(), Some("foo"));
    }

    #[test]
    fn es_namespace_import() {
        let result = parse_js("import * as ns from 'module-name';");
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].alias.as_deref(), Some("ns"));
    }

    #[test]
    fn require_import() {
        let result = parse_js("const x = require('module-name');");
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].raw_path, "module-name");
        assert_eq!(result.imports[0].kind, "require");
    }

    #[test]
    fn dynamic_import() {
        let result = parse_js("const x = import('module-name');");
        let dyn_imports: Vec<_> = result
            .imports
            .iter()
            .filter(|i| i.kind == "dynamic_import")
            .collect();
        assert_eq!(dyn_imports.len(), 1);
        assert_eq!(dyn_imports[0].raw_path, "module-name");
    }

    #[test]
    fn re_export() {
        let result = parse_js("export { foo } from 'module-name';");
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].kind, "re_export");
        assert_eq!(result.imports[0].raw_path, "module-name");
    }

    #[test]
    fn jsx_component_ref() {
        let result = parse_js("function App() { return <Foo />; }");
        let refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.name == "Foo")
            .collect();
        assert!(!refs.is_empty(), "should find JSX component ref");
    }

    #[test]
    fn definition_names_not_in_refs() {
        let result = parse_js("function myFunc(param) { const x = 1; class MyClass {} }");
        let ref_names: Vec<_> = result.references.iter().map(|r| r.name.as_str()).collect();
        assert!(!ref_names.contains(&"myFunc"), "function name is a def");
        assert!(!ref_names.contains(&"param"), "param is a def");
        assert!(!ref_names.contains(&"x"), "variable name is a def");
        assert!(!ref_names.contains(&"MyClass"), "class name is a def");
    }

    #[test]
    fn side_effect_import() {
        let result = parse_js("import 'polyfill';");
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].raw_path, "polyfill");
        assert_eq!(result.imports[0].kind, "es_import");
        assert!(result.imports[0].alias.is_none());
    }
}
