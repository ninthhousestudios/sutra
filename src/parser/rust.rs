use crate::error::{Result, SutraError};
use crate::parser::{
    ExtractedImport, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, SymbolKind,
};
use tree_sitter::{Node, Parser, TreeCursor};

pub fn parse(source: &str, file_path: &str) -> Result<ParseResult> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|e| SutraError::Parse(format!("failed to set language: {e}")))?;

    let tree = parser
        .parse(source, None)
        .ok_or_else(|| SutraError::Parse("tree-sitter returned no tree".to_string()))?;

    let root = tree.root_node();
    let parsed_ok = !root.has_error();
    let src = source.as_bytes();

    // First pass: extract symbols
    let mut symbols = Vec::new();
    collect_symbols(&mut symbols, root, src, &[]);

    // Second pass: extract references
    let mut references = Vec::new();
    collect_references(&mut references, root, src);

    // Third pass: extract imports
    let mut imports = Vec::new();
    collect_imports(&mut imports, root, src);

    Ok(ParseResult {
        file_path: file_path.to_string(),
        language: "rust".to_string(),
        symbols,
        references,
        imports,
        parsed_ok,
        line_count: source.lines().count(),
    })
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
                if let Some(sym) = extract_symbol(child, src, name_context, kind) {
                    symbols.push(sym);
                }
            }
            "struct_item" => {
                if let Some(sym) = extract_symbol(child, src, name_context, SymbolKind::Struct) {
                    symbols.push(sym);
                }
            }
            "enum_item" => {
                if let Some(sym) = extract_symbol(child, src, name_context, SymbolKind::Enum) {
                    symbols.push(sym);
                }
            }
            "trait_item" => {
                if let Some(sym) = extract_symbol(child, src, name_context, SymbolKind::Trait) {
                    let name = sym.short_name.clone();
                    symbols.push(sym);
                    // Recurse into trait body
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(name);
                        collect_symbols(symbols, body, src, &ctx);
                    }
                }
                continue; // already recursed
            }
            "impl_item" => {
                if let Some(sym) = extract_impl_symbol(child, src, name_context) {
                    let impl_name = sym.short_name.clone();
                    symbols.push(sym);
                    // Recurse into impl body — methods inside become Method
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(impl_name);
                        collect_symbols(symbols, body, src, &ctx);
                    }
                }
                continue; // already recursed
            }
            "type_item" => {
                if let Some(sym) =
                    extract_symbol(child, src, name_context, SymbolKind::TypeAlias)
                {
                    symbols.push(sym);
                }
            }
            "const_item" => {
                if let Some(sym) = extract_symbol(child, src, name_context, SymbolKind::Const) {
                    symbols.push(sym);
                }
            }
            "static_item" => {
                if let Some(sym) = extract_symbol(child, src, name_context, SymbolKind::Static) {
                    symbols.push(sym);
                }
            }
            "macro_definition" => {
                if let Some(sym) = extract_symbol(child, src, name_context, SymbolKind::Macro) {
                    symbols.push(sym);
                }
            }
            "mod_item" => {
                if let Some(sym) = extract_symbol(child, src, name_context, SymbolKind::Module) {
                    let name = sym.short_name.clone();
                    symbols.push(sym);
                    // Recurse into module body if it has one (inline module)
                    if let Some(body) = child.child_by_field_name("body") {
                        let mut ctx = name_context.to_vec();
                        ctx.push(name);
                        collect_symbols(symbols, body, src, &ctx);
                    }
                }
                continue; // already recursed
            }
            _ => {
                // Recurse into other nodes (e.g., declaration_list inside impls)
                collect_symbols(symbols, child, src, name_context);
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
    })
}

/// Extract an impl symbol — name is derived from the type (and optionally the trait).
fn extract_impl_symbol(
    node: Node,
    src: &[u8],
    name_context: &[String],
) -> Option<ExtractedSymbol> {
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

fn extract_signature(
    node: Node,
    src: &[u8],
    kind: SymbolKind,
) -> (Option<String>, Option<String>) {
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
            // Strip the "use " prefix and trailing ";"
            let raw = text
                .strip_prefix("use ")
                .unwrap_or(text)
                .strip_suffix(';')
                .unwrap_or(text)
                .trim();
            imports.push(ExtractedImport {
                raw_path: raw.to_string(),
                line: node.start_position().row + 1,
            });
        }
        return; // don't recurse into use_declaration children
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_parse_function() {
        let src = "fn hello() {}";
        let result = parse(src, "test.rs").unwrap();
        assert!(result.parsed_ok);
        assert_eq!(result.symbols.len(), 1);
        assert_eq!(result.symbols[0].short_name, "hello");
        assert_eq!(result.symbols[0].kind, SymbolKind::Function);
    }
}
