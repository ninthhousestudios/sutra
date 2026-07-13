use crate::error::Result;
use crate::parser::adapter::ParseContext;
use crate::parser::{
    ExtractedImport, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, SymbolKind,
    complexity, structural_hash,
};
use tree_sitter::{Node, TreeCursor};

// ---------------------------------------------------------------------------
// Scope arena — built at parse time, used for file-local ref resolution
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Module,
    Impl,
    Function,
    Block,
}

#[derive(Debug)]
struct Scope {
    parent: Option<usize>,
    /// Indices into the flat symbols slice for defs directly in this scope
    defs: Vec<usize>,
    /// Local let-bindings that shadow but don't resolve cross-file
    bindings: Vec<String>,
    kind: ScopeKind,
    start_line: usize,
    end_line: usize,
}

pub const LOCAL_BINDING_SENTINEL: &str = "::local_binding::";

fn build_scope_arena(root: Node, src: &[u8], symbols: &[&ExtractedSymbol]) -> Vec<Scope> {
    let mut arena: Vec<Scope> = Vec::new();

    arena.push(Scope {
        parent: None,
        defs: Vec::new(),
        bindings: Vec::new(),
        kind: ScopeKind::Module,
        start_line: 1,
        end_line: root.end_position().row + 1,
    });

    build_scopes_recursive(root, src, 0, &mut arena);

    for (sym_idx, sym) in symbols.iter().enumerate() {
        let scope_idx = find_tightest_scope(&arena, sym.start_line);
        arena[scope_idx].defs.push(sym_idx);
    }

    arena
}

fn build_scopes_recursive(node: Node, src: &[u8], parent_idx: usize, arena: &mut Vec<Scope>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "mod_item" | "impl_item" | "trait_item" => {
                let kind = match child.kind() {
                    "mod_item" => ScopeKind::Module,
                    _ => ScopeKind::Impl,
                };
                if let Some(body) = child.child_by_field_name("body") {
                    let idx = arena.len();
                    arena.push(Scope {
                        parent: Some(parent_idx),
                        defs: Vec::new(),
                        bindings: Vec::new(),
                        kind,
                        start_line: body.start_position().row + 1,
                        end_line: body.end_position().row + 1,
                    });
                    build_scopes_recursive(body, src, idx, arena);
                }
            }
            "function_item" | "function_signature_item" => {
                if let Some(body) = child.child_by_field_name("body") {
                    let idx = arena.len();
                    arena.push(Scope {
                        parent: Some(parent_idx),
                        defs: Vec::new(),
                        bindings: Vec::new(),
                        kind: ScopeKind::Function,
                        start_line: body.start_position().row + 1,
                        end_line: body.end_position().row + 1,
                    });
                    collect_let_bindings(body, src, &mut arena[idx].bindings);
                    build_scopes_recursive(body, src, idx, arena);
                }
            }
            "block" if is_nested_block(child) => {
                let idx = arena.len();
                arena.push(Scope {
                    parent: Some(parent_idx),
                    defs: Vec::new(),
                    bindings: Vec::new(),
                    kind: ScopeKind::Block,
                    start_line: child.start_position().row + 1,
                    end_line: child.end_position().row + 1,
                });
                collect_let_bindings(child, src, &mut arena[idx].bindings);
                build_scopes_recursive(child, src, idx, arena);
            }
            _ => {
                build_scopes_recursive(child, src, parent_idx, arena);
            }
        }
    }
}

fn is_nested_block(node: Node) -> bool {
    node.parent()
        .is_none_or(|p| !matches!(p.kind(), "function_item" | "function_signature_item"))
}

fn collect_let_bindings(block: Node, src: &[u8], bindings: &mut Vec<String>) {
    let mut cursor = block.walk();
    for child in block.children(&mut cursor) {
        if child.kind() == "let_declaration" {
            if let Some(pat) = child.child_by_field_name("pattern") {
                collect_pattern_names(pat, src, bindings);
            }
        }
    }
}

fn collect_pattern_names(pat: Node, src: &[u8], names: &mut Vec<String>) {
    match pat.kind() {
        "identifier" => {
            if let Ok(name) = pat.utf8_text(src) {
                if name != "_" {
                    names.push(name.to_string());
                }
            }
        }
        "tuple_pattern" | "slice_pattern" | "tuple_struct_pattern" | "struct_pattern" => {
            let mut cursor = pat.walk();
            for child in pat.children(&mut cursor) {
                collect_pattern_names(child, src, names);
            }
        }
        _ => {}
    }
}

fn find_tightest_scope(arena: &[Scope], line: usize) -> usize {
    let mut best = 0;
    let mut best_size = usize::MAX;
    for (i, scope) in arena.iter().enumerate() {
        if line >= scope.start_line && line <= scope.end_line {
            let size = scope.end_line - scope.start_line;
            if size < best_size {
                best_size = size;
                best = i;
            }
        }
    }
    best
}

fn resolve_refs_locally(arena: &[Scope], symbols: &[&ExtractedSymbol], refs: &mut [ExtractedRef]) {
    for r in refs.iter_mut() {
        if matches!(r.context_kind, RefContextKind::Import) {
            continue;
        }
        let scope_idx = find_tightest_scope(arena, r.line);
        r.resolved_local_target = resolve_in_scope_chain(arena, symbols, scope_idx, &r.name);
    }
}

fn resolve_in_scope_chain(
    arena: &[Scope],
    symbols: &[&ExtractedSymbol],
    start: usize,
    name: &str,
) -> Option<String> {
    let mut idx = start;
    loop {
        let scope = &arena[idx];

        if matches!(scope.kind, ScopeKind::Function | ScopeKind::Block)
            && scope.bindings.iter().any(|b| b == name)
        {
            return Some(LOCAL_BINDING_SENTINEL.to_string());
        }

        if let Some(&sym_idx) = scope
            .defs
            .iter()
            .find(|&&si| symbols[si].short_name == name)
        {
            return Some(symbols[sym_idx].qualified_name.to_string());
        }

        match scope.parent {
            Some(p) => idx = p,
            None => return None,
        }
    }
}

// ---------------------------------------------------------------------------

pub fn parse(ctx: &ParseContext) -> Result<ParseResult> {
    let root = ctx.tree.root_node();
    let parsed_ok = !root.has_error();
    let src = ctx.source;

    let symbols = collect_symbols(root, src, &[]);

    let mut references = Vec::new();
    collect_references(&mut references, root, src);

    let flat_syms = crate::parser::flatten_symbols(&symbols);
    let arena = build_scope_arena(root, src, &flat_syms);
    resolve_refs_locally(&arena, &flat_syms, &mut references);

    let mut imports = Vec::new();
    collect_imports(&mut imports, root, src);

    Ok(ParseResult {
        file_path: ctx.file_path.to_string(),
        language: "rust".to_string(),
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
pub const FLAG_CFG_TEST: u32 = 0x02;
pub const FLAG_FFI_ENTRY: u32 = 0x04;
pub const FLAG_OVERRIDE: u32 = 0x08;

fn extract_flags(node: Node, src: &[u8]) -> u32 {
    let mut flags = 0u32;
    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        if s.kind() == "attribute_item" {
            let text = s.utf8_text(src).unwrap_or("");
            if text.contains("#[test]")
                || text.contains("tokio::test")
                || text.contains("async_std::test")
                || text.contains("#[bench]")
            {
                flags |= FLAG_TEST;
            }
            if text.contains("no_mangle")
                || text.contains("export_name")
                || text.contains("wasm_bindgen")
                || text.contains("pyfunction")
                || text.contains("pyo3")
            {
                flags |= FLAG_FFI_ENTRY;
            }
        } else if s.kind() != "line_comment" && s.kind() != "block_comment" {
            break;
        }
        sib = s.prev_sibling();
    }
    flags
}

fn has_cfg_test_attr(node: Node, src: &[u8]) -> bool {
    let mut sib = node.prev_sibling();
    while let Some(s) = sib {
        if s.kind() == "attribute_item" {
            let text = s.utf8_text(src).unwrap_or("");
            if text.contains("cfg(test)") || text.contains("cfg( test )") {
                return true;
            }
        } else if s.kind() != "line_comment" && s.kind() != "block_comment" {
            break;
        }
        sib = s.prev_sibling();
    }
    false
}

// ---------------------------------------------------------------------------
// Symbol extraction
// ---------------------------------------------------------------------------

/// Recursively walk the tree collecting symbol definitions.
/// `name_context` carries the qualified-name prefix built from enclosing scopes.
fn collect_symbols(node: Node, src: &[u8], name_context: &[String]) -> Vec<ExtractedSymbol> {
    collect_symbols_inner(node, src, name_context, false)
}

fn collect_symbols_inner(
    node: Node,
    src: &[u8],
    name_context: &[String],
    in_cfg_test: bool,
) -> Vec<ExtractedSymbol> {
    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "function_item" | "function_signature_item" => {
                let inside_impl = name_context
                    .last()
                    .is_some_and(|_| node.kind() == "declaration_list");
                let kind = if inside_impl || is_inside_impl(node) {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                if let Some(mut sym) = extract_symbol(child, src, name_context, kind) {
                    sym.flags |= extract_flags(child, src);
                    if in_cfg_test {
                        sym.flags |= FLAG_CFG_TEST;
                    }
                    symbols.push(sym);
                }
            }
            "struct_item" => {
                if let Some(mut sym) = extract_symbol(child, src, name_context, SymbolKind::Struct)
                {
                    sym.flags |= extract_flags(child, src);
                    if in_cfg_test {
                        sym.flags |= FLAG_CFG_TEST;
                    }
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(sym.short_name.clone());
                        sym.children = extract_field_symbols(body, src, &ctx);
                    }
                    symbols.push(sym);
                }
                continue;
            }
            "enum_item" => {
                if let Some(mut sym) = extract_symbol(child, src, name_context, SymbolKind::Enum) {
                    sym.flags |= extract_flags(child, src);
                    if in_cfg_test {
                        sym.flags |= FLAG_CFG_TEST;
                    }
                    symbols.push(sym);
                }
            }
            "trait_item" => {
                if let Some(mut sym) = extract_symbol(child, src, name_context, SymbolKind::Trait) {
                    sym.flags |= extract_flags(child, src);
                    if in_cfg_test {
                        sym.flags |= FLAG_CFG_TEST;
                    }
                    let name = sym.short_name.clone();
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(name);
                        sym.children = collect_symbols_inner(body, src, &ctx, in_cfg_test);
                    }
                    symbols.push(sym);
                }
                continue;
            }
            "impl_item" => {
                if let Some(mut sym) = extract_impl_symbol(child, src, name_context) {
                    sym.flags |= extract_flags(child, src);
                    if in_cfg_test {
                        sym.flags |= FLAG_CFG_TEST;
                    }
                    let impl_name = sym.short_name.clone();
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(impl_name);
                        sym.children = collect_symbols_inner(body, src, &ctx, in_cfg_test);
                    }
                    symbols.push(sym);
                }
                continue;
            }
            "type_item" => {
                if let Some(mut sym) =
                    extract_symbol(child, src, name_context, SymbolKind::TypeAlias)
                {
                    sym.flags |= extract_flags(child, src);
                    if in_cfg_test {
                        sym.flags |= FLAG_CFG_TEST;
                    }
                    symbols.push(sym);
                }
            }
            "const_item" => {
                if let Some(mut sym) = extract_symbol(child, src, name_context, SymbolKind::Const) {
                    sym.flags |= extract_flags(child, src);
                    if in_cfg_test {
                        sym.flags |= FLAG_CFG_TEST;
                    }
                    symbols.push(sym);
                }
            }
            "static_item" => {
                if let Some(mut sym) = extract_symbol(child, src, name_context, SymbolKind::Static)
                {
                    sym.flags |= extract_flags(child, src);
                    if in_cfg_test {
                        sym.flags |= FLAG_CFG_TEST;
                    }
                    symbols.push(sym);
                }
            }
            "macro_definition" => {
                if let Some(mut sym) = extract_symbol(child, src, name_context, SymbolKind::Macro) {
                    sym.flags |= extract_flags(child, src);
                    if in_cfg_test {
                        sym.flags |= FLAG_CFG_TEST;
                    }
                    symbols.push(sym);
                }
            }
            "mod_item" => {
                let child_cfg_test = in_cfg_test || has_cfg_test_attr(child, src);
                if let Some(mut sym) = extract_symbol(child, src, name_context, SymbolKind::Module)
                {
                    sym.flags |= extract_flags(child, src);
                    if child_cfg_test {
                        sym.flags |= FLAG_CFG_TEST;
                    }
                    let name = sym.short_name.clone();
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(name);
                        sym.children = collect_symbols_inner(body, src, &ctx, child_cfg_test);
                    }
                    symbols.push(sym);
                }
                continue;
            }
            _ => {
                symbols.extend(collect_symbols_inner(child, src, name_context, in_cfg_test));
            }
        }
    }
    symbols
}

/// Check whether a node is (transitively) inside an impl_item's body.
fn is_inside_impl(node: Node) -> bool {
    let mut current = Some(node);
    while let Some(n) = current {
        if n.kind() == "impl_item" {
            return true;
        }
        current = n.parent();
    }
    false
}

/// Extract a symbol from a definition node.
fn extract_language_attrs(node: Node, src: &[u8], kind: SymbolKind) -> Option<String> {
    let mut attrs = serde_json::Map::new();

    let modifiers_contain = |kw: &str| -> bool {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "function_modifiers" {
                let mut mcursor = child.walk();
                for m in child.children(&mut mcursor) {
                    if m.kind() == kw {
                        return true;
                    }
                }
            }
            if child.kind() == kw {
                return true;
            }
        }
        false
    };

    match kind {
        SymbolKind::Function | SymbolKind::Method => {
            if modifiers_contain("async") {
                attrs.insert("is_async".into(), true.into());
            }
            if modifiers_contain("unsafe") {
                attrs.insert("is_unsafe".into(), true.into());
            }

            if let Some(ret) = node.child_by_field_name("return_type") {
                let type_node = if ret.kind() == "generic_type" {
                    ret.child_by_field_name("type")
                } else {
                    Some(ret)
                };
                if let Some(tn) = type_node {
                    let name = match tn.kind() {
                        "type_identifier" => tn.utf8_text(src).ok(),
                        "scoped_type_identifier" => {
                            let mut c = tn.walk();
                            tn.children(&mut c)
                                .filter(|ch| ch.kind() == "type_identifier")
                                .last()
                                .and_then(|ch| ch.utf8_text(src).ok())
                        }
                        "reference_type" => {
                            let mut c = tn.walk();
                            tn.named_children(&mut c)
                                .find(|c| c.kind() == "type_identifier")
                                .and_then(|c| c.utf8_text(src).ok())
                        }
                        _ => None,
                    };
                    match name {
                        Some("Result") => {
                            attrs.insert("returns_result".into(), true.into());
                        }
                        Some("Option") => {
                            attrs.insert("returns_option".into(), true.into());
                        }
                        Some("Self") => {
                            attrs.insert("returns_self".into(), true.into());
                        }
                        _ => {}
                    }
                }
            }

            if let Some(params) = node.child_by_field_name("parameters") {
                let mut pcursor = params.walk();
                for child in params.children(&mut pcursor) {
                    if child.kind() == "self_parameter" {
                        let text = child.utf8_text(src).unwrap_or("");
                        if text.contains("&mut") {
                            attrs.insert("takes_self_mut".into(), true.into());
                        } else if text.contains('&') {
                            attrs.insert("takes_self_ref".into(), true.into());
                        }
                        break;
                    }
                }
            }

            if let Some(type_params) = node.child_by_field_name("type_parameters") {
                let tp_text = type_params.utf8_text(src).unwrap_or("");
                if tp_text.contains('\'') {
                    attrs.insert("has_lifetime_params".into(), true.into());
                }
            }
        }
        SymbolKind::Impl | SymbolKind::Trait if modifiers_contain("unsafe") => {
            attrs.insert("is_unsafe".into(), true.into());
        }
        SymbolKind::Struct | SymbolKind::Enum => {
            if let Some(type_params) = node.child_by_field_name("type_parameters") {
                let tp_text = type_params.utf8_text(src).unwrap_or("");
                if tp_text.contains('\'') {
                    attrs.insert("has_lifetime_params".into(), true.into());
                }
            }
        }
        _ => {}
    }

    Some(serde_json::to_string(&attrs).unwrap_or_else(|_| "{}".into()))
}

fn extract_symbol(
    node: Node,
    src: &[u8],
    name_context: &[String],
    kind: SymbolKind,
) -> Option<ExtractedSymbol> {
    let short_name = node_name_text(node, src)?;

    let name_range = node
        .child_by_field_name("name")
        .map(|n| (n.start_byte(), n.end_byte()));
    let structural_hash = Some(structural_hash::compute(node, src, name_range));

    let qualified_name = build_qualified_name(name_context, &short_name);

    let visibility = extract_visibility(node, src);
    let docstring = extract_docstring(node, src);
    let (signature, signature_hash) = extract_signature(node, src, kind);
    let language_attrs = extract_language_attrs(node, src, kind);

    let (cyclomatic, cognitive, max_nesting) =
        if matches!(kind, SymbolKind::Function | SymbolKind::Method) {
            if let Some(body) = node.child_by_field_name("body") {
                (
                    Some(complexity::cyclomatic(body, src, "rust")),
                    Some(complexity::cognitive(body, src, "rust")),
                    Some(complexity::max_nesting_depth(body, src, "rust")),
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
        language_attrs,
    })
}

fn extract_field_symbols(body: Node, src: &[u8], name_context: &[String]) -> Vec<ExtractedSymbol> {
    let mut fields = Vec::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "field_declaration" {
            continue;
        }
        let Some(name_node) = child.child_by_field_name("name") else {
            continue;
        };
        let Ok(field_name) = name_node.utf8_text(src) else {
            continue;
        };
        let qualified_name = build_qualified_name(name_context, field_name);
        let visibility = extract_visibility(child, src);
        let type_text = child
            .child_by_field_name("type")
            .and_then(|t| t.utf8_text(src).ok());
        let signature = type_text.map(|t| format!("{field_name}: {t}"));
        let signature_hash = signature
            .as_ref()
            .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
        let docstring = extract_docstring(child, src);

        let sh = Some(structural_hash::compute(
            child,
            src,
            Some((name_node.start_byte(), name_node.end_byte())),
        ));

        fields.push(ExtractedSymbol {
            qualified_name,
            short_name: field_name.to_string(),
            kind: SymbolKind::Field,
            signature,
            signature_hash,
            structural_hash: sh,
            visibility,
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

/// Extract an impl symbol — name is derived from the type (and optionally the trait).
fn extract_impl_symbol(node: Node, src: &[u8], name_context: &[String]) -> Option<ExtractedSymbol> {
    // impl Trait for Type  or  impl Type
    // We need to find the type being implemented.
    let impl_name = derive_impl_name(node, src)?;

    let qualified_name = build_qualified_name(name_context, &impl_name);

    let visibility = extract_visibility(node, src);
    let docstring = extract_docstring(node, src);
    let language_attrs = extract_language_attrs(node, src, SymbolKind::Impl);

    let sh = Some(structural_hash::compute(node, src, None));

    Some(ExtractedSymbol {
        qualified_name,
        short_name: impl_name,
        kind: SymbolKind::Impl,
        signature: None,
        signature_hash: None,
        structural_hash: sh,
        visibility,
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
        language_attrs,
    })
}

/// Derive the "name" of an impl block.
/// For `impl Foo`, returns "Foo".
/// For `impl Bar for Foo`, returns "Foo" (the type being implemented).
fn derive_impl_name(node: Node, src: &[u8]) -> Option<String> {
    // tree-sitter-rust impl_item has field "type" for the implementing type
    // and field "trait" for the trait being implemented (if any).
    // The "type" field holds the concrete type.
    if let Some(type_node) = node.child_by_field_name("type") {
        Some(type_node.utf8_text(src).ok()?.to_string())
    } else {
        // Fallback: look for a type_identifier child
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "type_identifier" {
                return Some(child.utf8_text(src).ok()?.to_string());
            }
        }
        None
    }
}

/// Get the text of a node's `name` field child.
fn node_name_text(node: Node, src: &[u8]) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name("name") {
        name_node.utf8_text(src).ok().map(|s| s.to_string())
    } else {
        // For macro_definition the name child may use a different field
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "identifier" || child.kind() == "type_identifier" {
                return child.utf8_text(src).ok().map(|s| s.to_string());
            }
        }
        None
    }
}

fn build_qualified_name(context: &[String], name: &str) -> String {
    if context.is_empty() {
        name.to_string()
    } else {
        format!("{}::{}", context.join("::"), name)
    }
}

fn extract_visibility(node: Node, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return child.utf8_text(src).ok().map(|s| s.to_string());
        }
    }
    None
}

fn extract_docstring(node: Node, src: &[u8]) -> Option<String> {
    let mut doc_lines: Vec<String> = Vec::new();

    // Walk preceding siblings to find doc comments
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        let kind = sib.kind();
        if let Ok(text) = sib.utf8_text(src) {
            if kind == "line_comment" && (text.starts_with("///") || text.starts_with("//!")) {
                // Strip the comment prefix
                let content = if let Some(s) = text.strip_prefix("/// ") {
                    s
                } else if let Some(s) = text.strip_prefix("///") {
                    s
                } else if let Some(s) = text.strip_prefix("//! ") {
                    s
                } else {
                    text.strip_prefix("//!").unwrap_or(text)
                };
                doc_lines.push(content.to_string());
                sibling = sib.prev_sibling();
                continue;
            } else if kind == "block_comment" && text.starts_with("/**") {
                // Block doc comment — take the whole thing minus delimiters
                let inner = text
                    .strip_prefix("/**")
                    .unwrap_or(text)
                    .strip_suffix("*/")
                    .unwrap_or(text)
                    .trim();
                doc_lines.push(inner.to_string());
                sibling = sib.prev_sibling();
                continue;
            } else if kind == "attribute_item" || kind == "attribute" {
                // Attributes like #[derive(...)] can appear between doc comments and the item
                sibling = sib.prev_sibling();
                continue;
            }
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

fn extract_signature(node: Node, src: &[u8], kind: SymbolKind) -> (Option<String>, Option<String>) {
    match kind {
        SymbolKind::Function | SymbolKind::Method => {
            let sig = build_fn_signature(node, src);
            let hash = sig
                .as_ref()
                .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
            (sig, hash)
        }
        _ => (None, None),
    }
}

/// Build a function signature string from its parameters and return type.
fn build_fn_signature(node: Node, src: &[u8]) -> Option<String> {
    let name = node_name_text(node, src).unwrap_or_default();
    let params_node = node.child_by_field_name("parameters");
    let params_text = params_node
        .and_then(|n| n.utf8_text(src).ok())
        .unwrap_or("()");

    let ret_type = node
        .child_by_field_name("return_type")
        .and_then(|n| n.utf8_text(src).ok());

    let mut sig = format!("fn {name}{params_text}");
    if let Some(rt) = ret_type {
        sig.push_str(&format!(" -> {rt}"));
    }

    // Also check for type_parameters (generics)
    if let Some(tp) = node.child_by_field_name("type_parameters")
        && let Ok(tp_text) = tp.utf8_text(src)
    {
        let insert_pos = "fn ".len() + name.len();
        sig.insert_str(insert_pos, tp_text);
    }

    Some(sig)
}

// ---------------------------------------------------------------------------
// Reference extraction
// ---------------------------------------------------------------------------

/// Collect identifier and type_identifier references, skipping definition names.
fn collect_references(refs: &mut Vec<ExtractedRef>, node: Node, src: &[u8]) {
    let mut cursor = node.walk();
    walk_refs_recursive(refs, &mut cursor, src);
}

fn walk_refs_recursive(refs: &mut Vec<ExtractedRef>, cursor: &mut TreeCursor, src: &[u8]) {
    let node = cursor.node();
    let kind = node.kind();

    if (kind == "identifier" || kind == "type_identifier")
        && !is_definition_name(node)
        && let Ok(name) = node.utf8_text(src)
    {
        let context_kind = classify_ref_context(node);
        if context_kind != RefContextKind::Other {
            refs.push(ExtractedRef {
                name: name.to_string(),
                line: node.start_position().row + 1,
                col: node.start_position().column,
                context_kind,
                resolved_local_target: None,
            });
        }
    }

    // Method names in call position: foo.method()
    if kind == "field_identifier"
        && is_method_call_name(node)
        && let Ok(name) = node.utf8_text(src)
    {
        refs.push(ExtractedRef {
            name: name.to_string(),
            line: node.start_position().row + 1,
            col: node.start_position().column,
            context_kind: RefContextKind::Call,
            resolved_local_target: None,
        });
    }

    // Recurse into children
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

/// Returns true if this node is the `name` child of a definition node.
fn is_definition_name(node: Node) -> bool {
    if let Some(parent) = node.parent() {
        let parent_kind = parent.kind();
        let is_def = matches!(
            parent_kind,
            "function_item"
                | "function_signature_item"
                | "struct_item"
                | "enum_item"
                | "trait_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "macro_definition"
                | "mod_item"
        );
        if is_def {
            // Check that this node is actually the `name` field
            if let Some(name_node) = parent.child_by_field_name("name") {
                return name_node.id() == node.id();
            }
        }
    }
    false
}

fn is_method_call_name(node: Node) -> bool {
    node.parent()
        .filter(|p| p.kind() == "field_expression")
        .and_then(|p| p.parent())
        .is_some_and(|gp| gp.kind() == "call_expression")
}

/// Classify a reference by its parent context.
fn classify_ref_context(node: Node) -> RefContextKind {
    if let Some(parent) = node.parent() {
        let pk = parent.kind();
        match pk {
            "call_expression" => return RefContextKind::Call,
            "use_declaration" | "use_as_clause" | "scoped_identifier" | "use_wildcard"
            | "use_list" | "scoped_use_list"
                if is_inside_use(node) =>
            {
                return RefContextKind::Import;
            }
            "field_expression" if node.kind() == "field_identifier" => {
                return RefContextKind::FieldAccess;
            }
            _ => {}
        }

        // Construction: only type_identifier nodes that are the name of a struct_expression.
        // Covers Foo { .. }, inner::Foo { .. }, and Foo::<T> { .. }.
        if node.kind() == "type_identifier" && is_struct_expression_name(node) {
            return RefContextKind::Construction;
        }

        // Type use: type_identifier used in type contexts
        if node.kind() == "type_identifier" {
            return RefContextKind::TypeUse;
        }

        // Also check if parent is a type-related node
        if matches!(
            pk,
            "type_identifier"
                | "generic_type"
                | "type_arguments"
                | "type_bound"
                | "function_type"
                | "reference_type"
                | "pointer_type"
                | "array_type"
                | "slice_type"
                | "tuple_type"
        ) {
            return RefContextKind::TypeUse;
        }
    }

    RefContextKind::Other
}

/// Walk ancestors from a type_identifier to see if it's the name of a struct_expression.
/// Handles: Foo { .. }, inner::Foo { .. }, Foo::<T> { .. }, inner::Foo::<T> { .. }.
fn is_struct_expression_name(node: Node) -> bool {
    let mut current = node;
    while let Some(parent) = current.parent() {
        match parent.kind() {
            "struct_expression" => return true,
            "scoped_type_identifier" | "generic_type_with_turbofish" => {
                current = parent;
            }
            _ => return false,
        }
    }
    false
}

fn is_inside_use(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        if n.kind() == "use_declaration" {
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
    walk_imports_recursive(imports, &mut cursor, src, &[]);
}

fn walk_imports_recursive(
    imports: &mut Vec<ExtractedImport>,
    cursor: &mut TreeCursor,
    src: &[u8],
    inline_mod_prefix: &[&str],
) {
    let node = cursor.node();

    if node.kind() == "use_declaration" {
        if let Ok(text) = node.utf8_text(src) {
            let raw = text
                .strip_prefix("use ")
                .unwrap_or(text)
                .strip_suffix(';')
                .unwrap_or(text)
                .trim();
            let line = node.start_position().row + 1;
            for path in expand_braced_import(raw) {
                imports.push(ExtractedImport {
                    raw_path: path,
                    line,
                    kind: "import",
                });
            }
        }
        return;
    }

    if node.kind() == "mod_item"
        && let Some(name_node) = node.child_by_field_name("name")
        && let Ok(name) = name_node.utf8_text(src)
    {
        if node.child_by_field_name("body").is_none() {
            let mut path = String::from("self");
            for seg in inline_mod_prefix {
                path.push_str("::");
                path.push_str(seg);
            }
            path.push_str("::");
            path.push_str(name);
            let line = node.start_position().row + 1;
            imports.push(ExtractedImport {
                raw_path: path,
                line,
                kind: "mod",
            });
            return;
        }
        let mut nested_prefix = inline_mod_prefix.to_vec();
        nested_prefix.push(name);
        if cursor.goto_first_child() {
            loop {
                walk_imports_recursive(imports, cursor, src, &nested_prefix);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
            cursor.goto_parent();
        }
        return;
    }

    if cursor.goto_first_child() {
        loop {
            walk_imports_recursive(imports, cursor, src, inline_mod_prefix);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// Expand `std::collections::{HashMap, HashSet}` into individual paths.
/// Handles `self` (e.g. `std::{self, io}` → `std` + `std::io`) and
/// nested braces (one level deep).
fn expand_braced_import(raw: &str) -> Vec<String> {
    let Some(brace_start) = raw.find('{') else {
        return vec![raw.to_string()];
    };
    let Some(brace_end) = raw.rfind('}') else {
        return vec![raw.to_string()];
    };

    let prefix = raw[..brace_start].trim_end_matches("::");
    let inner = &raw[brace_start + 1..brace_end];

    let mut results = Vec::new();
    for item in split_top_level(inner) {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if item == "self" {
            results.push(prefix.to_string());
        } else if item.contains('{') {
            // Nested brace group — recurse
            let nested = format!("{prefix}::{item}");
            results.extend(expand_braced_import(&nested));
        } else {
            results.push(format!("{prefix}::{item}"));
        }
    }
    results
}

/// Split on commas at brace depth 0 (handles nested braces).
fn split_top_level(s: &str) -> Vec<&str> {
    let mut results = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                results.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if start < s.len() {
        results.push(&s[start..]);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::adapter::{ParserPool, RustAdapter};
    use std::time::Duration;

    fn parse_rust(source: &str, file_path: &str) -> crate::error::Result<ParseResult> {
        let adapter = RustAdapter;
        let mut pool = ParserPool::new(Duration::from_secs(5));
        pool.parse_with(&adapter, source, file_path)
    }

    #[test]
    fn smoke_parse_function() {
        let src = "fn hello() {}";
        let result = parse_rust(src, "test.rs").unwrap();
        assert!(result.parsed_ok);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].short_name, "hello");
        assert_eq!(result.symbols[0].kind, SymbolKind::Function);
    }

    #[test]
    fn flag_detects_test_attr() {
        let src = "#[test]\nfn my_test() {}";
        let result = parse_rust(src, "test.rs").unwrap();
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].flags & FLAG_TEST, FLAG_TEST);
    }

    #[test]
    fn flag_detects_tokio_test() {
        let src = "#[tokio::test]\nasync fn my_test() {}";
        let result = parse_rust(src, "test.rs").unwrap();
        assert_eq!(result.symbols[0].flags & FLAG_TEST, FLAG_TEST);
    }

    #[test]
    fn flag_detects_cfg_test_module() {
        let src = "#[cfg(test)]\nmod tests {\n    fn helper() {}\n}";
        let result = parse_rust(src, "test.rs").unwrap();
        let module = result
            .symbols
            .iter()
            .find(|s| s.short_name == "tests")
            .unwrap();
        assert_eq!(module.flags & FLAG_CFG_TEST, FLAG_CFG_TEST);
        let flat = crate::parser::flatten_symbols(&result.symbols);
        let helper = flat.iter().find(|s| s.short_name == "helper").unwrap();
        assert_eq!(helper.flags & FLAG_CFG_TEST, FLAG_CFG_TEST);
    }

    #[test]
    fn flag_detects_no_mangle() {
        let src = "#[no_mangle]\npub extern \"C\" fn ffi_entry() {}";
        let result = parse_rust(src, "test.rs").unwrap();
        assert_eq!(result.symbols[0].flags & FLAG_FFI_ENTRY, FLAG_FFI_ENTRY);
    }

    #[test]
    fn no_flags_on_normal_function() {
        let src = "pub fn normal() {}";
        let result = parse_rust(src, "test.rs").unwrap();
        assert_eq!(result.symbols[0].flags, 0);
    }

    #[test]
    fn method_call_extracts_call_ref() {
        let src = "fn go(db: Db) { db.query(); }";
        let result = parse_rust(src, "test.rs").unwrap();
        let query_ref = result
            .references
            .iter()
            .find(|r| r.name == "query")
            .expect("method name should be extracted");
        assert_eq!(query_ref.context_kind, RefContextKind::Call);
    }

    #[test]
    fn receiver_not_extracted() {
        let src = "fn go(db: Db) { db.query(); }";
        let result = parse_rust(src, "test.rs").unwrap();
        assert!(
            !result.references.iter().any(|r| r.name == "db"),
            "receiver (Other) should be suppressed"
        );
    }

    #[test]
    fn chained_method_calls() {
        let src = "fn go(a: A) { a.b().c(); }";
        let result = parse_rust(src, "test.rs").unwrap();
        let names: Vec<&str> = result
            .references
            .iter()
            .filter(|r| {
                r.context_kind == RefContextKind::Call && ["b", "c"].contains(&r.name.as_str())
            })
            .map(|r| r.name.as_str())
            .collect();
        assert!(names.contains(&"b"), "b should be a Call ref");
        assert!(names.contains(&"c"), "c should be a Call ref");
        assert!(
            !result.references.iter().any(|r| r.name == "a"),
            "receiver a (Other) should be suppressed"
        );
    }

    #[test]
    fn plain_field_access_no_method_ref() {
        let src = "fn go(foo: Foo) { let _ = foo.bar; }";
        let result = parse_rust(src, "test.rs").unwrap();
        assert!(
            !result.references.iter().any(|r| r.name == "bar"),
            "plain field access should not produce a ref"
        );
        assert!(
            !result.references.iter().any(|r| r.name == "foo"),
            "receiver (Other) in field access should be suppressed"
        );
    }

    #[test]
    fn self_method_call() {
        let src = "impl Foo { fn go(&self) { self.method(); } }";
        let result = parse_rust(src, "test.rs").unwrap();
        let method_ref = result
            .references
            .iter()
            .find(|r| r.name == "method")
            .expect("method name after self should be extracted");
        assert_eq!(method_ref.context_kind, RefContextKind::Call);
        assert!(
            !result.references.iter().any(|r| r.name == "self"),
            "self should not be extracted as a ref"
        );
    }

    #[test]
    fn struct_fields_extracted() {
        let src = "pub struct Config {\n    pub name: String,\n    port: u16,\n}";
        let result = parse_rust(src, "test.rs").unwrap();
        assert_eq!(result.symbols.len(), 1);
        let config = &result.symbols[0];
        assert_eq!(config.kind, SymbolKind::Struct);
        assert_eq!(config.children.len(), 2);

        let name_field = &config.children[0];
        assert_eq!(name_field.short_name, "name");
        assert_eq!(name_field.kind, SymbolKind::Field);
        assert_eq!(name_field.qualified_name, "Config::name");
        assert_eq!(name_field.signature.as_deref(), Some("name: String"));
        assert_eq!(name_field.visibility.as_deref(), Some("pub"));

        let port_field = &config.children[1];
        assert_eq!(port_field.short_name, "port");
        assert_eq!(port_field.kind, SymbolKind::Field);
        assert_eq!(port_field.qualified_name, "Config::port");
        assert_eq!(port_field.signature.as_deref(), Some("port: u16"));
        assert_eq!(port_field.visibility, None);
    }

    #[test]
    fn tuple_struct_no_fields() {
        let src = "struct Wrapper(u32);";
        let result = parse_rust(src, "test.rs").unwrap();
        assert_eq!(result.symbols.len(), 1);
        assert!(result.symbols[0].children.is_empty());
    }

    #[test]
    fn nested_module_struct_fields() {
        let src = "mod inner {\n    pub struct Point {\n        pub x: f64,\n        pub y: f64,\n    }\n}";
        let result = parse_rust(src, "test.rs").unwrap();
        let flat = crate::parser::flatten_symbols(&result.symbols);
        let x = flat.iter().find(|s| s.short_name == "x").unwrap();
        assert_eq!(x.kind, SymbolKind::Field);
        assert_eq!(x.qualified_name, "inner::Point::x");
    }

    #[test]
    fn mod_declaration_extracted_as_import() {
        let result = parse_rust("mod foo;\nmod bar;\n", "src/lib.rs").unwrap();
        assert_eq!(result.imports.len(), 2);
        assert_eq!(result.imports[0].raw_path, "self::foo");
        assert_eq!(result.imports[0].kind, "mod");
        assert_eq!(result.imports[1].raw_path, "self::bar");
        assert_eq!(result.imports[1].kind, "mod");
    }

    #[test]
    fn inline_mod_no_import() {
        let result = parse_rust("mod foo { fn inner() {} }\n", "src/lib.rs").unwrap();
        assert!(result.imports.is_empty());
    }

    #[test]
    fn mod_and_use_both_extracted() {
        let result = parse_rust("mod foo;\nuse crate::foo::Thing;\n", "src/lib.rs").unwrap();
        assert_eq!(result.imports.len(), 2);
        let kinds: Vec<&str> = result.imports.iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&"mod"));
        assert!(kinds.contains(&"import"));
    }

    #[test]
    fn nested_mod_inside_inline_module() {
        let result = parse_rust("mod outer {\n    mod inner;\n}\n", "src/lib.rs").unwrap();
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].raw_path, "self::outer::inner");
        assert_eq!(result.imports[0].kind, "mod");
    }

    #[test]
    fn deeply_nested_mod_inside_inline_modules() {
        let result = parse_rust(
            "mod a {\n    mod b {\n        mod c;\n    }\n}\n",
            "src/lib.rs",
        )
        .unwrap();
        assert_eq!(result.imports.len(), 1);
        assert_eq!(result.imports[0].raw_path, "self::a::b::c");
    }
}
