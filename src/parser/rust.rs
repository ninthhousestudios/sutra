use crate::error::Result;
use crate::parser::{
    ExtractedImport, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, SymbolKind,
    complexity,
};
use crate::parser::adapter::ParseContext;
use tree_sitter::{Node, TreeCursor};

pub fn parse(ctx: &ParseContext) -> Result<ParseResult> {
    let root = ctx.tree.root_node();
    let parsed_ok = !root.has_error();
    let src = ctx.source;

    let mut symbols = Vec::new();
    collect_symbols(&mut symbols, root, src, &[]);

    let mut references = Vec::new();
    collect_references(&mut references, root, src);

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
fn collect_symbols(
    symbols: &mut Vec<ExtractedSymbol>,
    node: Node,
    src: &[u8],
    name_context: &[String],
) {
    collect_symbols_inner(symbols, node, src, name_context, false);
}

fn collect_symbols_inner(
    symbols: &mut Vec<ExtractedSymbol>,
    node: Node,
    src: &[u8],
    name_context: &[String],
    in_cfg_test: bool,
) {
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
                    symbols.push(sym);
                }
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
                    symbols.push(sym);
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(name);
                        collect_symbols_inner(symbols, body, src, &ctx, in_cfg_test);
                    }
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
                    symbols.push(sym);
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(impl_name);
                        collect_symbols_inner(symbols, body, src, &ctx, in_cfg_test);
                    }
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
                    symbols.push(sym);
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(name);
                        collect_symbols_inner(symbols, body, src, &ctx, child_cfg_test);
                    }
                }
                continue;
            }
            _ => {
                collect_symbols_inner(symbols, child, src, name_context, in_cfg_test);
            }
        }
    }
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
fn extract_symbol(
    node: Node,
    src: &[u8],
    name_context: &[String],
    kind: SymbolKind,
) -> Option<ExtractedSymbol> {
    let short_name = node_name_text(node, src)?;

    let qualified_name = build_qualified_name(name_context, &short_name);
    let parent_qn = if name_context.is_empty() {
        None
    } else {
        Some(name_context.join("::"))
    };

    let visibility = extract_visibility(node, src);
    let docstring = extract_docstring(node, src);
    let (signature, signature_hash) = extract_signature(node, src, kind);

    let (cyclomatic, cognitive) = if matches!(kind, SymbolKind::Function | SymbolKind::Method) {
        if let Some(body) = node.child_by_field_name("body") {
            (
                Some(complexity::cyclomatic(body, src, "rust")),
                Some(complexity::cognitive(body, src, "rust")),
            )
        } else {
            (Some(1), Some(0))
        }
    } else {
        (None, None)
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
        parent_qualified_name: parent_qn,
        docstring,
        cyclomatic,
        cognitive,
        flags: 0,
    })
}

/// Extract an impl symbol — name is derived from the type (and optionally the trait).
fn extract_impl_symbol(node: Node, src: &[u8], name_context: &[String]) -> Option<ExtractedSymbol> {
    // impl Trait for Type  or  impl Type
    // We need to find the type being implemented.
    let impl_name = derive_impl_name(node, src)?;

    let qualified_name = build_qualified_name(name_context, &impl_name);
    let parent_qn = if name_context.is_empty() {
        None
    } else {
        Some(name_context.join("::"))
    };

    let visibility = extract_visibility(node, src);
    let docstring = extract_docstring(node, src);

    Some(ExtractedSymbol {
        qualified_name,
        short_name: impl_name,
        kind: SymbolKind::Impl,
        signature: None,
        signature_hash: None,
        visibility,
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        parent_qualified_name: parent_qn,
        docstring,
        cyclomatic: None,
        cognitive: None,
        flags: 0,
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
        refs.push(ExtractedRef {
            name: name.to_string(),
            line: node.start_position().row + 1,
            col: node.start_position().column,
            context_kind,
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
            "field_expression" => return RefContextKind::FieldAccess,
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
    walk_imports_recursive(imports, &mut cursor, src);
}

fn walk_imports_recursive(imports: &mut Vec<ExtractedImport>, cursor: &mut TreeCursor, src: &[u8]) {
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
                });
            }
        }
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
        let helper = result
            .symbols
            .iter()
            .find(|s| s.short_name == "helper")
            .unwrap();
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
}
