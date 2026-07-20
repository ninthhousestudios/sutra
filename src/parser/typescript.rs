use crate::error::Result;
use crate::parser::adapter::ParseContext;
use crate::parser::javascript;
use crate::parser::{
    ExtractedImport, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, SymbolKind,
    structural_hash,
};
use tree_sitter::{Node, TreeCursor};

const FLAG_OVERRIDE: u32 = 0x02;

pub fn parse(ctx: &ParseContext) -> Result<ParseResult> {
    let root = ctx.tree.root_node();
    let parsed_ok = !root.has_error();
    let src = ctx.source;
    let file_path = ctx.file_path;

    let symbols = collect_symbols(root, src, file_path);

    let mut references = Vec::new();
    javascript::collect_references(&mut references, root, src);
    collect_type_references(&mut references, root, src);

    let flat_syms = crate::parser::flatten_symbols(&symbols);
    let arena = javascript::build_scope_arena(root, src, &flat_syms);
    javascript::resolve_refs_locally(&arena, &flat_syms, &mut references);

    let mut imports = Vec::new();
    javascript::collect_imports(&mut imports, root, src);
    mark_type_imports(&mut imports, root, src);

    Ok(ParseResult {
        file_path: file_path.to_string(),
        language: "typescript".to_string(),
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
// Symbol collection — routes JS nodes to JS extractors, TS nodes to TS extractors
// ---------------------------------------------------------------------------

fn collect_symbols(root: Node, src: &[u8], file_path: &str) -> Vec<ExtractedSymbol> {
    collect_symbols_inner(root, src, file_path, &[])
}

fn collect_symbols_inner(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
) -> Vec<ExtractedSymbol> {
    let mut symbols = Vec::new();
    for child in node.children(&mut node.walk()) {
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                if let Some(mut sym) =
                    javascript::extract_function(child, src, file_path, name_context)
                {
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx: Vec<&str> = name_context.to_vec();
                        ctx.push(&sym.short_name);
                        sym.children = collect_symbols_inner(body, src, file_path, &ctx);
                    }
                    symbols.push(sym);
                }
            }
            "class_declaration" => {
                if let Some(sym) = extract_class(child, src, file_path, name_context, false) {
                    symbols.push(sym);
                }
            }
            "abstract_class_declaration" => {
                if let Some(sym) = extract_class(child, src, file_path, name_context, true) {
                    symbols.push(sym);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                javascript::extract_variable_declarators(
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
            "interface_declaration" => {
                if let Some(sym) = extract_interface(child, src, file_path, name_context) {
                    symbols.push(sym);
                }
            }
            "type_alias_declaration" => {
                if let Some(sym) = extract_type_alias(child, src, file_path, name_context) {
                    symbols.push(sym);
                }
            }
            "enum_declaration" => {
                if let Some(sym) = extract_enum(child, src, file_path, name_context) {
                    symbols.push(sym);
                }
            }
            "internal_module" => {
                if let Some(sym) = extract_namespace(child, src, file_path, name_context) {
                    symbols.push(sym);
                }
            }
            _ => {
                symbols.extend(collect_symbols_inner(child, src, file_path, name_context));
            }
        }
    }
    symbols
}

// ---------------------------------------------------------------------------
// Interface extraction → Trait
// ---------------------------------------------------------------------------

fn extract_interface(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = javascript::node_text(name_node, src);
    let qualified_name = javascript::build_qualified_name(name_context, &name);

    let docstring = javascript::extract_jsdoc(node, src);
    let flags = javascript::extract_flags(node, src, file_path);
    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );

    let mut children = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut ctx: Vec<&str> = name_context.to_vec();
        ctx.push(&name);
        extract_interface_body(body, src, file_path, &ctx, &mut children);
    }

    Some(ExtractedSymbol {
        qualified_name,
        short_name: name,
        kind: SymbolKind::Trait,
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

fn extract_interface_body(
    body: Node,
    src: &[u8],
    file_path: &str,
    ctx: &[&str],
    children: &mut Vec<ExtractedSymbol>,
) {
    for child in body.children(&mut body.walk()) {
        match child.kind() {
            "property_signature" => {
                if let Some(sym) = extract_property_signature(child, src, ctx) {
                    children.push(sym);
                }
            }
            "method_signature" => {
                if let Some(sym) = extract_method_signature(child, src, file_path, ctx) {
                    children.push(sym);
                }
            }
            _ => {}
        }
    }
}

fn extract_property_signature(
    node: Node,
    src: &[u8],
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = javascript::node_text(name_node, src);
    let qualified_name = javascript::build_qualified_name(name_context, &name);

    let is_readonly = javascript::has_keyword(node, src, "readonly");
    let is_optional = node
        .children(&mut node.walk())
        .any(|c| !c.is_named() && c.utf8_text(src) == Ok("?"));

    let mut attrs = serde_json::Map::new();
    if is_readonly {
        attrs.insert("readonly".into(), true.into());
    }
    if is_optional {
        attrs.insert("optional".into(), true.into());
    }

    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );

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
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: javascript::attrs_to_json(&attrs),
    })
}

fn extract_method_signature(
    node: Node,
    src: &[u8],
    _file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = javascript::node_text(name_node, src);
    let qualified_name = javascript::build_qualified_name(name_context, &name);

    let is_optional = node
        .children(&mut node.walk())
        .any(|c| !c.is_named() && c.utf8_text(src) == Ok("?"));

    let mut attrs = serde_json::Map::new();
    if is_optional {
        attrs.insert("optional".into(), true.into());
    }

    let sig = node.child_by_field_name("parameters").and_then(|params| {
        let params_text = params.utf8_text(src).ok()?;
        Some(format!("{name}{params_text}"))
    });
    let sig_hash = sig
        .as_ref()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );

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
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: javascript::attrs_to_json(&attrs),
    })
}

// ---------------------------------------------------------------------------
// Type alias extraction → TypeAlias
// ---------------------------------------------------------------------------

fn extract_type_alias(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = javascript::node_text(name_node, src);
    let qualified_name = javascript::build_qualified_name(name_context, &name);

    let docstring = javascript::extract_jsdoc(node, src);
    let flags = javascript::extract_flags(node, src, file_path);
    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );

    Some(ExtractedSymbol {
        qualified_name,
        short_name: name,
        kind: SymbolKind::TypeAlias,
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
        flags,
        language_attrs: None,
    })
}

// ---------------------------------------------------------------------------
// Enum extraction → Enum with Field children
// ---------------------------------------------------------------------------

fn extract_enum(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = javascript::node_text(name_node, src);
    let qualified_name = javascript::build_qualified_name(name_context, &name);

    let docstring = javascript::extract_jsdoc(node, src);
    let flags = javascript::extract_flags(node, src, file_path);
    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );

    let is_const = javascript::has_keyword(node, src, "const");
    let mut attrs = serde_json::Map::new();
    if is_const {
        attrs.insert("const".into(), true.into());
    }

    let mut children = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut ctx: Vec<&str> = name_context.to_vec();
        ctx.push(&name);
        for member in body.children(&mut body.walk()) {
            let mn = if member.kind() == "enum_member" {
                member.child_by_field_name("name")
            } else if member.kind() == "property_identifier" {
                Some(member)
            } else {
                continue;
            };
            let Some(mn) = mn else { continue };
            let mname = javascript::node_text(mn, src);
            let mq = javascript::build_qualified_name(&ctx, &mname);
            let anchor = if member.kind() == "enum_member" {
                member
            } else {
                mn
            };
            let mhash = structural_hash::compute(anchor, src, None);
            children.push(ExtractedSymbol {
                qualified_name: mq,
                short_name: mname,
                kind: SymbolKind::Field,
                signature: None,
                signature_hash: None,
                structural_hash: Some(mhash),
                visibility: None,
                start_line: anchor.start_position().row + 1,
                start_col: anchor.start_position().column,
                end_line: anchor.end_position().row + 1,
                end_col: anchor.end_position().column,
                children: Vec::new(),
                parent_symbol_id: None,
                docstring: None,
                cyclomatic: None,
                cognitive: None,
                max_nesting: None,
                flags: 0,
                language_attrs: None,
            });
        }
    }

    Some(ExtractedSymbol {
        qualified_name,
        short_name: name,
        kind: SymbolKind::Enum,
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
        language_attrs: javascript::attrs_to_json(&attrs),
    })
}

// ---------------------------------------------------------------------------
// Namespace extraction → Module
// ---------------------------------------------------------------------------

fn extract_namespace(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = javascript::node_text(name_node, src);
    let qualified_name = javascript::build_qualified_name(name_context, &name);

    let docstring = javascript::extract_jsdoc(node, src);
    let flags = javascript::extract_flags(node, src, file_path);
    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );

    let mut children = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut ctx: Vec<&str> = name_context.to_vec();
        ctx.push(&name);
        children = collect_symbols_inner(body, src, file_path, &ctx);
    }

    Some(ExtractedSymbol {
        qualified_name,
        short_name: name,
        kind: SymbolKind::Module,
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

// ---------------------------------------------------------------------------
// Class extraction — reuses JS helpers but adds TS modifiers
// ---------------------------------------------------------------------------

fn extract_class(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
    is_abstract: bool,
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = javascript::node_text(name_node, src);
    let qualified_name = javascript::build_qualified_name(name_context, &name);

    let docstring = javascript::extract_jsdoc(node, src);
    let flags = javascript::extract_flags(node, src, file_path);
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

    let mut attrs = serde_json::Map::new();
    if is_abstract {
        attrs.insert("abstract".into(), true.into());
    }
    collect_decorators(node, src, &mut attrs);

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
        language_attrs: javascript::attrs_to_json(&attrs),
    })
}

fn extract_class_body(
    body: Node,
    src: &[u8],
    file_path: &str,
    ctx: &[&str],
    children: &mut Vec<ExtractedSymbol>,
) {
    for child in body.children(&mut body.walk()) {
        match child.kind() {
            "method_definition" | "abstract_method_definition" => {
                if let Some(mut method) = extract_method(child, src, file_path, ctx) {
                    if let Some(method_body) = child.child_by_field_name("body") {
                        let mut mctx: Vec<&str> = ctx.to_vec();
                        mctx.push(&method.short_name);
                        method.children = collect_symbols_inner(method_body, src, file_path, &mctx);
                    }
                    children.push(method);
                }
            }
            "field_definition" | "public_field_definition" => {
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

fn extract_method(
    node: Node,
    src: &[u8],
    file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    let mut sym = javascript::extract_method(node, src, file_path, name_context)?;

    let mut attrs: serde_json::Map<String, serde_json::Value> = sym
        .language_attrs
        .as_ref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();

    if let Some(vis) = get_accessibility(node, src) {
        sym.visibility = Some(vis);
    }

    if node.kind() == "abstract_method_definition" || has_modifier(node, "abstract") {
        attrs.insert("abstract".into(), true.into());
    }
    if has_modifier(node, "readonly") {
        attrs.insert("readonly".into(), true.into());
    }
    if has_modifier(node, "override") {
        attrs.insert("override".into(), true.into());
        sym.flags |= FLAG_OVERRIDE;
    }
    if has_modifier(node, "declare") {
        attrs.insert("declare".into(), true.into());
    }

    collect_decorators(node, src, &mut attrs);
    sym.language_attrs = javascript::attrs_to_json(&attrs);
    Some(sym)
}

fn extract_field(
    node: Node,
    src: &[u8],
    _file_path: &str,
    name_context: &[&str],
) -> Option<ExtractedSymbol> {
    // TS uses "name" field; JS uses "property" field — try both
    let name_node = node
        .child_by_field_name("property")
        .or_else(|| node.child_by_field_name("name"))?;
    let name = javascript::node_text(name_node, src);
    let qualified_name = javascript::build_qualified_name(name_context, &name);

    let is_static = javascript::has_keyword(node, src, "static");
    let is_computed = name_node.kind() == "computed_property_name";

    let mut attrs = serde_json::Map::new();
    if is_static {
        attrs.insert("static".into(), true.into());
    }
    if is_computed {
        attrs.insert("computed".into(), true.into());
    }

    if has_modifier(node, "readonly") {
        attrs.insert("readonly".into(), true.into());
    }
    if has_modifier(node, "override") {
        attrs.insert("override".into(), true.into());
    }
    if has_modifier(node, "declare") {
        attrs.insert("declare".into(), true.into());
    }
    if has_modifier(node, "abstract") {
        attrs.insert("abstract".into(), true.into());
    }

    collect_decorators(node, src, &mut attrs);

    let struct_hash = structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    );
    let docstring = javascript::extract_jsdoc(node, src);
    let flags = if has_modifier(node, "override") {
        FLAG_OVERRIDE
    } else {
        0
    };

    Some(ExtractedSymbol {
        qualified_name,
        short_name: name,
        kind: SymbolKind::Field,
        signature: None,
        signature_hash: None,
        structural_hash: Some(struct_hash),
        visibility: get_accessibility(node, src),
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
        flags,
        language_attrs: javascript::attrs_to_json(&attrs),
    })
}

// ---------------------------------------------------------------------------
// Export handling — JS exports + TS-specific declarations
// ---------------------------------------------------------------------------

fn handle_export(
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
                if let Some(mut sym) =
                    javascript::extract_function(child, src, file_path, name_context)
                {
                    sym.visibility = Some(vis.to_string());
                    if sym.docstring.is_none() {
                        sym.docstring = javascript::extract_jsdoc(node, src);
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
                if let Some(mut sym) = extract_class(child, src, file_path, name_context, false) {
                    sym.visibility = Some(vis.to_string());
                    if sym.docstring.is_none() {
                        sym.docstring = javascript::extract_jsdoc(node, src);
                    }
                    symbols.push(sym);
                }
            }
            "abstract_class_declaration" => {
                if let Some(mut sym) = extract_class(child, src, file_path, name_context, true) {
                    sym.visibility = Some(vis.to_string());
                    if sym.docstring.is_none() {
                        sym.docstring = javascript::extract_jsdoc(node, src);
                    }
                    symbols.push(sym);
                }
            }
            "interface_declaration" => {
                if let Some(mut sym) = extract_interface(child, src, file_path, name_context) {
                    sym.visibility = Some(vis.to_string());
                    if sym.docstring.is_none() {
                        sym.docstring = javascript::extract_jsdoc(node, src);
                    }
                    symbols.push(sym);
                }
            }
            "type_alias_declaration" => {
                if let Some(mut sym) = extract_type_alias(child, src, file_path, name_context) {
                    sym.visibility = Some(vis.to_string());
                    if sym.docstring.is_none() {
                        sym.docstring = javascript::extract_jsdoc(node, src);
                    }
                    symbols.push(sym);
                }
            }
            "enum_declaration" => {
                if let Some(mut sym) = extract_enum(child, src, file_path, name_context) {
                    sym.visibility = Some(vis.to_string());
                    if sym.docstring.is_none() {
                        sym.docstring = javascript::extract_jsdoc(node, src);
                    }
                    symbols.push(sym);
                }
            }
            "internal_module" => {
                if let Some(mut sym) = extract_namespace(child, src, file_path, name_context) {
                    sym.visibility = Some(vis.to_string());
                    symbols.push(sym);
                }
            }
            "lexical_declaration" | "variable_declaration" => {
                javascript::extract_variable_declarators(
                    child,
                    src,
                    file_path,
                    name_context,
                    Some(vis),
                    symbols,
                );
                if let Some(last) = symbols.last_mut() {
                    if last.docstring.is_none() {
                        last.docstring = javascript::extract_jsdoc(node, src);
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
// TS modifier helpers
// ---------------------------------------------------------------------------

fn get_accessibility(node: Node, src: &[u8]) -> Option<String> {
    for child in node.children(&mut node.walk()) {
        if child.kind() == "accessibility_modifier" {
            return child.utf8_text(src).ok().map(|s| s.to_string());
        }
    }
    None
}

fn has_modifier(node: Node, modifier: &str) -> bool {
    node.children(&mut node.walk()).any(|c| {
        c.kind() == modifier || (modifier == "override" && c.kind() == "override_modifier")
    })
}

fn collect_decorators(
    node: Node,
    src: &[u8],
    attrs: &mut serde_json::Map<String, serde_json::Value>,
) {
    let mut decorators = Vec::new();
    for child in node.children(&mut node.walk()) {
        if child.kind() == "decorator" {
            if let Ok(text) = child.utf8_text(src) {
                let name = text.trim_start_matches('@');
                let name = name.split('(').next().unwrap_or(name);
                decorators.push(serde_json::Value::String(name.to_string()));
            }
        }
    }
    if !decorators.is_empty() {
        attrs.insert("decorators".into(), serde_json::Value::Array(decorators));
    }
}

// ---------------------------------------------------------------------------
// Type reference extraction — type_identifier nodes → TypeUse refs
// ---------------------------------------------------------------------------

fn collect_type_references(refs: &mut Vec<ExtractedRef>, node: Node, src: &[u8]) {
    let mut cursor = node.walk();
    walk_type_refs(&mut cursor, src, refs);
}

fn walk_type_refs(cursor: &mut TreeCursor, src: &[u8], refs: &mut Vec<ExtractedRef>) {
    let node = cursor.node();

    if node.kind() == "type_identifier" && !is_type_definition(node) {
        if let Ok(name) = node.utf8_text(src) {
            let receiver = node.parent().and_then(|p| {
                if p.kind() == "nested_type_identifier" {
                    p.child_by_field_name("module")
                        .and_then(|m| m.utf8_text(src).ok())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            });
            refs.push(ExtractedRef {
                name: name.to_string(),
                line: node.start_position().row + 1,
                col: node.start_position().column,
                context_kind: RefContextKind::TypeUse,
                resolved_local_target: None,
                receiver,
            });
        }
    }

    if cursor.goto_first_child() {
        loop {
            walk_type_refs(cursor, src, refs);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn is_type_definition(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "interface_declaration" | "type_alias_declaration" | "enum_declaration" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        "type_parameter" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Type import/re-export marking
// ---------------------------------------------------------------------------

fn mark_type_imports(imports: &mut [ExtractedImport], root: Node, src: &[u8]) {
    let mut cursor = root.walk();
    walk_type_import_markers(&mut cursor, src, imports);
}

fn walk_type_import_markers(cursor: &mut TreeCursor, src: &[u8], imports: &mut [ExtractedImport]) {
    let node = cursor.node();

    match node.kind() {
        "import_statement" => {
            if has_type_keyword(node, src) {
                let line = node.start_position().row + 1;
                for imp in imports.iter_mut() {
                    if imp.line == line && imp.kind == "es_import" {
                        imp.kind = "type_import";
                    }
                }
            }
            return;
        }
        "export_statement" => {
            if node.child_by_field_name("source").is_some() && has_type_keyword(node, src) {
                let line = node.start_position().row + 1;
                for imp in imports.iter_mut() {
                    if imp.line == line && imp.kind == "re_export" {
                        imp.kind = "type_re_export";
                    }
                }
            }
        }
        _ => {}
    }

    if cursor.goto_first_child() {
        loop {
            walk_type_import_markers(cursor, src, imports);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn has_type_keyword(node: Node, src: &[u8]) -> bool {
    let mut found_lead = false;
    for child in node.children(&mut node.walk()) {
        if !child.is_named() {
            let text = child.utf8_text(src).unwrap_or("");
            if text == "import" || text == "export" {
                found_lead = true;
            } else if text == "type" && found_lead {
                return true;
            }
        }
        if child.is_named() {
            break;
        }
    }
    false
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
            "typescript"
        }
        fn extensions(&self) -> &[&str] {
            &["ts", "tsx"]
        }
        fn grammar(&self) -> tree_sitter::Language {
            tree_sitter_typescript::LANGUAGE_TSX.into()
        }
        fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
            super::parse(ctx)
        }
    }

    fn parse_ts(source: &str) -> ParseResult {
        let mut pool = ParserPool::new(Duration::from_secs(5));
        pool.parse_with(&Adapter, source, "test.ts").unwrap()
    }

    fn parse_tsx(source: &str) -> ParseResult {
        let mut pool = ParserPool::new(Duration::from_secs(5));
        pool.parse_with(&Adapter, source, "test.tsx").unwrap()
    }

    // -- Interface extraction ------------------------------------------------

    #[test]
    fn interface_declaration() {
        let result = parse_ts(
            "interface Greeter {
                readonly name: string;
                greet(msg: string): void;
            }",
        );
        assert_eq!(result.symbols.len(), 1);
        let iface = &result.symbols[0];
        assert_eq!(iface.short_name, "Greeter");
        assert_eq!(iface.kind, SymbolKind::Trait);
        assert_eq!(iface.children.len(), 2);

        let prop = &iface.children[0];
        assert_eq!(prop.short_name, "name");
        assert_eq!(prop.kind, SymbolKind::Field);
        let attrs: serde_json::Value =
            serde_json::from_str(prop.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(attrs["readonly"], true);

        let method = &iface.children[1];
        assert_eq!(method.short_name, "greet");
        assert_eq!(method.kind, SymbolKind::Method);
        assert!(method.signature.is_some());
    }

    // -- Type alias extraction -----------------------------------------------

    #[test]
    fn type_alias() {
        let result = parse_ts("type ID = string | number;");
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].short_name, "ID");
        assert_eq!(result.symbols[0].kind, SymbolKind::TypeAlias);
    }

    // -- Enum extraction -----------------------------------------------------

    #[test]
    fn enum_declaration() {
        let result = parse_ts(
            "enum Color {
                Red,
                Green,
                Blue
            }",
        );
        assert_eq!(result.symbols.len(), 1);
        let e = &result.symbols[0];
        assert_eq!(e.short_name, "Color");
        assert_eq!(e.kind, SymbolKind::Enum);
        assert_eq!(e.children.len(), 3);
        assert_eq!(e.children[0].short_name, "Red");
        assert_eq!(e.children[0].kind, SymbolKind::Field);
        assert_eq!(e.children[0].qualified_name, "Color::Red");
    }

    #[test]
    fn const_enum() {
        let result = parse_ts("const enum Dir { Up, Down }");
        let e = &result.symbols[0];
        assert_eq!(e.kind, SymbolKind::Enum);
        let attrs: serde_json::Value =
            serde_json::from_str(e.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(attrs["const"], true);
    }

    // -- Namespace extraction ------------------------------------------------

    #[test]
    fn namespace_declaration() {
        let result = parse_ts(
            "namespace Utils {
                function helper(): void {}
            }",
        );
        assert_eq!(result.symbols.len(), 1);
        let ns = &result.symbols[0];
        assert_eq!(ns.short_name, "Utils");
        assert_eq!(ns.kind, SymbolKind::Module);
        assert_eq!(ns.children.len(), 1);
        assert_eq!(ns.children[0].short_name, "helper");
        assert_eq!(ns.children[0].qualified_name, "Utils::helper");
    }

    // -- Abstract class ------------------------------------------------------

    #[test]
    fn abstract_class() {
        let result = parse_ts(
            "abstract class Shape {
                abstract area(): number;
                perimeter(): number { return 0; }
            }",
        );
        assert_eq!(result.symbols.len(), 1);
        let cls = &result.symbols[0];
        assert_eq!(cls.short_name, "Shape");
        assert_eq!(cls.kind, SymbolKind::Class);
        let attrs: serde_json::Value =
            serde_json::from_str(cls.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(attrs["abstract"], true);
    }

    // -- Access modifiers ----------------------------------------------------

    #[test]
    fn access_modifiers() {
        let result = parse_ts(
            "class Foo {
                public x: number = 1;
                private y: string = '';
                protected z: boolean = true;
            }",
        );
        let cls = &result.symbols[0];
        assert_eq!(cls.children.len(), 3);
        assert_eq!(cls.children[0].visibility.as_deref(), Some("public"));
        assert_eq!(cls.children[1].visibility.as_deref(), Some("private"));
        assert_eq!(cls.children[2].visibility.as_deref(), Some("protected"));
    }

    // -- Readonly modifier ---------------------------------------------------

    #[test]
    fn readonly_modifier() {
        let result = parse_ts(
            "class Foo {
                readonly id: number = 1;
            }",
        );
        let field = &result.symbols[0].children[0];
        let attrs: serde_json::Value =
            serde_json::from_str(field.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(attrs["readonly"], true);
    }

    // -- Override modifier ---------------------------------------------------

    #[test]
    fn override_modifier() {
        let result = parse_ts(
            "class Child extends Parent {
                override doSomething() {}
            }",
        );
        let method = &result.symbols[0].children[0];
        let attrs: serde_json::Value =
            serde_json::from_str(method.language_attrs.as_ref().unwrap()).unwrap();
        assert_eq!(attrs["override"], true);
        assert_ne!(method.flags & FLAG_OVERRIDE, 0);
    }

    // -- Decorators ----------------------------------------------------------

    #[test]
    fn class_decorator() {
        let result = parse_ts(
            "@Component
            class MyComponent {}",
        );
        let cls = &result.symbols[0];
        let attrs: serde_json::Value =
            serde_json::from_str(cls.language_attrs.as_ref().unwrap()).unwrap();
        let decorators = attrs["decorators"].as_array().unwrap();
        assert_eq!(decorators[0], "Component");
    }

    // -- Type annotation refs ------------------------------------------------

    #[test]
    fn type_annotation_refs() {
        let result = parse_ts("let x: Foo = getFoo();");
        let type_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(
            type_refs.iter().any(|r| r.name == "Foo"),
            "expected TypeUse ref to Foo, got: {type_refs:?}"
        );
    }

    // -- Generic type argument refs ------------------------------------------

    #[test]
    fn generic_type_refs() {
        let result = parse_ts("let items: Array<Item> = [];");
        let type_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(type_refs.iter().any(|r| r.name == "Array"));
        assert!(type_refs.iter().any(|r| r.name == "Item"));
    }

    // -- Implements clause refs ----------------------------------------------

    #[test]
    fn implements_refs() {
        let result = parse_ts("class Dog implements Animal, Pet {}");
        let type_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(type_refs.iter().any(|r| r.name == "Animal"));
        assert!(type_refs.iter().any(|r| r.name == "Pet"));
    }

    // -- Interface extends refs ----------------------------------------------

    #[test]
    fn interface_extends_refs() {
        let result = parse_ts("interface Admin extends User {}");
        let type_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(
            type_refs.iter().any(|r| r.name == "User"),
            "expected TypeUse ref to User, got: {type_refs:?}"
        );
    }

    // -- import type ---------------------------------------------------------

    #[test]
    fn import_type() {
        let result = parse_ts("import type { Foo } from 'bar';");
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].kind, "type_import");
        assert_eq!(result.imports[0].raw_path, "bar");
    }

    // -- export type (re-export) ---------------------------------------------

    #[test]
    fn type_re_export() {
        let result = parse_ts("export type { Foo } from 'bar';");
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].kind, "type_re_export");
    }

    // -- TSX parsing ---------------------------------------------------------

    #[test]
    fn tsx_jsx_element() {
        let result = parse_tsx(
            "function App(): JSX.Element {
                return <div className=\"app\">Hello</div>;
            }",
        );
        assert!(result.parsed_ok);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].short_name, "App");
    }

    #[test]
    fn tsx_with_type_annotation() {
        let result = parse_tsx(
            "const Comp: React.FC<Props> = (props) => {
                return <span>{props.name}</span>;
            };",
        );
        assert!(result.parsed_ok);
        assert_eq!(result.symbols.len(), 1);
        let type_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(type_refs.iter().any(|r| r.name == "FC"));
        assert!(type_refs.iter().any(|r| r.name == "Props"));
    }

    // -- Exported TS declarations --------------------------------------------

    #[test]
    fn export_interface() {
        let result = parse_ts("export interface Serializable { serialize(): string; }");
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].visibility.as_deref(), Some("export"));
        assert_eq!(result.symbols[0].kind, SymbolKind::Trait);
    }

    #[test]
    fn export_type_alias() {
        let result = parse_ts("export type Result<T> = T | Error;");
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].visibility.as_deref(), Some("export"));
        assert_eq!(result.symbols[0].kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn export_enum() {
        let result = parse_ts("export enum Status { Active, Inactive }");
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].visibility.as_deref(), Some("export"));
        assert_eq!(result.symbols[0].kind, SymbolKind::Enum);
    }

    // -- JS features still work through TS layer ----------------------------

    #[test]
    fn js_function_through_ts() {
        let result = parse_ts("function add(a: number, b: number): number { return a + b; }");
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].short_name, "add");
        assert_eq!(result.symbols[0].kind, SymbolKind::Function);
    }

    #[test]
    fn js_class_through_ts() {
        let result = parse_ts(
            "class Greeter {
                greeting: string;
                greet() { return this.greeting; }
            }",
        );
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].kind, SymbolKind::Class);
        assert_eq!(result.symbols[0].children.len(), 2);
    }

    #[test]
    fn test_file_detection() {
        let mut pool = ParserPool::new(Duration::from_secs(5));
        let result = pool
            .parse_with(&Adapter, "const x = 1;", "foo.test.ts")
            .unwrap();
        assert_ne!(result.symbols[0].flags & javascript::FLAG_TEST, 0);
    }

    // -- Type definitions not in refs ----------------------------------------

    #[test]
    fn type_definitions_not_in_refs() {
        let result = parse_ts(
            "interface Foo {}
             type Bar = string;
             enum Baz { A }",
        );
        let type_refs: Vec<_> = result
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(
            !type_refs.iter().any(|r| r.name == "Foo"),
            "Foo definition should not appear as ref"
        );
        assert!(
            !type_refs.iter().any(|r| r.name == "Bar"),
            "Bar definition should not appear as ref"
        );
    }
}
