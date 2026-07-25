use crate::error::Result;
use crate::parser::adapter::ParseContext;
use crate::parser::dart::TYPE_TRACKING_PREFIX;
use crate::parser::{
    ExtractedImport, ExtractedRef, ExtractedSymbol, ParseResult, RefContextKind, SymbolKind,
    complexity, structural_hash,
};
use tree_sitter::{Node, TreeCursor};

// ---------------------------------------------------------------------------
// Scope arena — LEGB resolution for Python
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    Module,
    Class,
    Function,
}

#[derive(Debug)]
struct Scope {
    parent: Option<usize>,
    defs: Vec<usize>,
    bindings: Vec<(String, usize)>,
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

    build_scopes_recursive(root, src, 0, &mut arena, symbols);

    arena
}

fn find_symbol_for_node(node: Node, src: &[u8], symbols: &[&ExtractedSymbol]) -> Option<usize> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(src).ok()?;
    let line = node.start_position().row + 1;
    symbols
        .iter()
        .position(|s| s.short_name == name && s.start_line == line)
}

fn find_symbol_for_decorated(
    dec_node: Node,
    src: &[u8],
    symbols: &[&ExtractedSymbol],
) -> Option<usize> {
    let mut cursor = dec_node.walk();
    for child in dec_node.children(&mut cursor) {
        if matches!(child.kind(), "function_definition" | "class_definition") {
            let name_node = child.child_by_field_name("name")?;
            let name = name_node.utf8_text(src).ok()?;
            let dec_line = dec_node.start_position().row + 1;
            return symbols
                .iter()
                .position(|s| s.short_name == name && s.start_line == dec_line);
        }
    }
    None
}

fn add_function_scope(
    func_node: Node,
    src: &[u8],
    parent_idx: usize,
    arena: &mut Vec<Scope>,
    symbols: &[&ExtractedSymbol],
) {
    if let Some(body) = func_node.child_by_field_name("body") {
        let idx = arena.len();
        arena.push(Scope {
            parent: Some(parent_idx),
            defs: Vec::new(),
            bindings: Vec::new(),
            kind: ScopeKind::Function,
            start_line: func_node.start_position().row + 1,
            end_line: func_node.end_position().row + 1,
        });
        collect_param_bindings(func_node, src, &mut arena[idx].bindings);
        collect_local_bindings(body, src, &mut arena[idx].bindings);
        build_scopes_recursive(body, src, idx, arena, symbols);
    }
}

fn add_class_scope(
    class_node: Node,
    src: &[u8],
    parent_idx: usize,
    arena: &mut Vec<Scope>,
    symbols: &[&ExtractedSymbol],
) {
    if let Some(body) = class_node.child_by_field_name("body") {
        let idx = arena.len();
        arena.push(Scope {
            parent: Some(parent_idx),
            defs: Vec::new(),
            bindings: Vec::new(),
            kind: ScopeKind::Class,
            start_line: body.start_position().row + 1,
            end_line: body.end_position().row + 1,
        });
        build_scopes_recursive(body, src, idx, arena, symbols);
    }
}

fn build_scopes_recursive(
    node: Node,
    src: &[u8],
    parent_idx: usize,
    arena: &mut Vec<Scope>,
    symbols: &[&ExtractedSymbol],
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let kind = child.kind();

        if matches!(kind, "function_definition" | "class_definition")
            && let Some(sym_idx) = find_symbol_for_node(child, src, symbols)
        {
            arena[parent_idx].defs.push(sym_idx);
        }

        match kind {
            "function_definition" => {
                add_function_scope(child, src, parent_idx, arena, symbols);
            }
            "class_definition" => {
                add_class_scope(child, src, parent_idx, arena, symbols);
            }
            "decorated_definition" => {
                let dec_node = child;
                if let Some(sym_idx) = find_symbol_for_decorated(dec_node, src, symbols) {
                    arena[parent_idx].defs.push(sym_idx);
                }
                let mut inner = dec_node.walk();
                for grandchild in dec_node.children(&mut inner) {
                    match grandchild.kind() {
                        "function_definition" => {
                            add_function_scope(grandchild, src, parent_idx, arena, symbols);
                        }
                        "class_definition" => {
                            add_class_scope(grandchild, src, parent_idx, arena, symbols);
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                build_scopes_recursive(child, src, parent_idx, arena, symbols);
            }
        }
    }
}

fn collect_param_bindings(func_node: Node, src: &[u8], bindings: &mut Vec<(String, usize)>) {
    let Some(params) = func_node.child_by_field_name("parameters") else {
        return;
    };
    let line = func_node.start_position().row + 1;
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                if let Ok(name) = child.utf8_text(src) {
                    bindings.push((name.to_string(), line));
                }
            }
            "typed_parameter" | "typed_default_parameter" | "default_parameter" => {
                if let Some(name_node) = child.child_by_field_name("name")
                    && let Ok(name) = name_node.utf8_text(src)
                {
                    bindings.push((name.to_string(), line));
                }
            }
            "list_splat_pattern" | "dictionary_splat_pattern" => {
                if let Some(inner) = child.named_child(0)
                    && inner.kind() == "identifier"
                    && let Ok(name) = inner.utf8_text(src)
                {
                    bindings.push((name.to_string(), line));
                }
            }
            _ => {}
        }
    }
}

fn collect_local_bindings(node: Node, src: &[u8], bindings: &mut Vec<(String, usize)>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let line = child.start_position().row + 1;
        match child.kind() {
            "expression_statement" => {
                let mut inner = child.walk();
                for grandchild in child.children(&mut inner) {
                    if grandchild.kind() == "assignment" {
                        collect_assignment_binding_names(grandchild, src, line, bindings);
                    }
                }
            }
            "for_statement" => {
                if let Some(left) = child.child_by_field_name("left") {
                    collect_binding_pattern_names(left, src, line, bindings);
                }
                if let Some(body) = child.child_by_field_name("body") {
                    collect_local_bindings(body, src, bindings);
                }
                if let Some(alt) = child.child_by_field_name("alternative") {
                    collect_local_bindings(alt, src, bindings);
                }
            }
            "while_statement" => {
                if let Some(body) = child.child_by_field_name("body") {
                    collect_local_bindings(body, src, bindings);
                }
                if let Some(alt) = child.child_by_field_name("alternative") {
                    collect_local_bindings(alt, src, bindings);
                }
            }
            "if_statement" => {
                collect_local_bindings(child, src, bindings);
            }
            "elif_clause" | "else_clause" => {
                if let Some(body) = child.child_by_field_name("body") {
                    collect_local_bindings(body, src, bindings);
                }
            }
            "with_statement" => {
                let mut inner = child.walk();
                for item in child.children(&mut inner) {
                    if item.kind() == "as_pattern"
                        && let Some(alias) = item.child_by_field_name("alias")
                        && alias.kind() == "identifier"
                        && let Ok(name) = alias.utf8_text(src)
                    {
                        bindings.push((name.to_string(), line));
                    }
                }
                if let Some(body) = child.child_by_field_name("body") {
                    collect_local_bindings(body, src, bindings);
                }
            }
            "try_statement" => {
                collect_except_bindings(child, src, bindings);
                collect_local_bindings(child, src, bindings);
            }
            "except_clause" | "finally_clause" => {
                if let Some(body) = child.child_by_field_name("body") {
                    collect_local_bindings(body, src, bindings);
                }
            }
            "block" => {
                collect_local_bindings(child, src, bindings);
            }
            // Stop recursing at nested function/class scopes
            "function_definition" | "class_definition" | "decorated_definition" => {}
            _ => {}
        }
    }
}

fn collect_assignment_binding_names(
    assign: Node,
    src: &[u8],
    line: usize,
    bindings: &mut Vec<(String, usize)>,
) {
    if let Some(left) = assign.child_by_field_name("left") {
        collect_binding_pattern_names(left, src, line, bindings);
    }
}

fn collect_binding_pattern_names(
    node: Node,
    src: &[u8],
    line: usize,
    bindings: &mut Vec<(String, usize)>,
) {
    match node.kind() {
        "identifier" => {
            if let Ok(name) = node.utf8_text(src)
                && name != "_"
            {
                bindings.push((name.to_string(), line));
            }
        }
        "tuple" | "list" | "pattern_list" | "tuple_pattern" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_binding_pattern_names(child, src, line, bindings);
            }
        }
        _ => {}
    }
}

fn collect_except_bindings(try_node: Node, src: &[u8], bindings: &mut Vec<(String, usize)>) {
    let mut cursor = try_node.walk();
    for child in try_node.children(&mut cursor) {
        if child.kind() == "except_clause" {
            let mut inner = child.walk();
            for grandchild in child.children(&mut inner) {
                if grandchild.kind() == "as_pattern"
                    && let Some(alias) = grandchild.child_by_field_name("alias")
                    && alias.kind() == "identifier"
                    && let Ok(name) = alias.utf8_text(src)
                {
                    bindings.push((name.to_string(), child.start_position().row + 1));
                }
            }
        }
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
        if r.resolved_local_target.is_some() {
            continue;
        }
        let scope_idx = find_tightest_scope(arena, r.line);
        r.resolved_local_target =
            resolve_in_scope_chain(arena, symbols, scope_idx, &r.name, r.line);
    }
}

fn resolve_in_scope_chain(
    arena: &[Scope],
    symbols: &[&ExtractedSymbol],
    start: usize,
    name: &str,
    ref_line: usize,
) -> Option<String> {
    let mut idx = start;
    loop {
        let scope = &arena[idx];

        // LEGB: class scopes are skipped for bare name lookups —
        // Python requires `self.attr` to access class members from methods.
        if scope.kind != ScopeKind::Class {
            if matches!(scope.kind, ScopeKind::Function)
                && scope
                    .bindings
                    .iter()
                    .any(|(b, decl_line)| b == name && ref_line >= *decl_line)
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
        }

        match scope.parent {
            Some(p) => idx = p,
            None => return None,
        }
    }
}

// ---------------------------------------------------------------------------
// Constructor type tracking for Python — `x = Foo()` → x has type Foo
// ---------------------------------------------------------------------------

struct PyTypeBinding {
    var_name: String,
    class_name: String,
    decl_line: usize,
    scope_end_line: usize,
}

fn collect_py_type_bindings(node: Node, src: &[u8], bindings: &mut Vec<PyTypeBinding>) {
    if node.kind() == "expression_statement" && !is_inside_class_body(node) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "assignment"
                && let Some(b) = extract_py_type_binding(child, src)
            {
                bindings.push(b);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_py_type_bindings(child, src, bindings);
    }
}

fn is_inside_class_body(node: Node) -> bool {
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "class_definition" => return true,
            "function_definition" => return false,
            _ => current = n.parent(),
        }
    }
    false
}

fn extract_py_type_binding(assign: Node, src: &[u8]) -> Option<PyTypeBinding> {
    let left = assign.child_by_field_name("left")?;
    if left.kind() != "identifier" {
        return None;
    }
    let var_name = left.utf8_text(src).ok()?;

    let right = assign.child_by_field_name("right")?;
    let class_name = py_constructor_type(right, src)?;

    let scope_end = enclosing_function_end_py(assign).unwrap_or(usize::MAX);

    Some(PyTypeBinding {
        var_name: var_name.to_string(),
        class_name,
        decl_line: assign.start_position().row + 1,
        scope_end_line: scope_end,
    })
}

fn py_constructor_type(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() == "call" {
        let func = node.child_by_field_name("function")?;
        let name = func.utf8_text(src).ok()?;
        if name.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
            return Some(name.to_string());
        }
    }
    None
}

fn enclosing_function_end_py(node: Node) -> Option<usize> {
    let mut current = node.parent();
    while let Some(n) = current {
        match n.kind() {
            "function_definition" => return Some(n.end_position().row + 1),
            "class_definition" => return None,
            _ => current = n.parent(),
        }
    }
    None
}

fn lookup_py_receiver_type<'a>(
    bindings: &'a [PyTypeBinding],
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

fn extract_py_call_receiver(node: Node, src: &[u8]) -> Option<String> {
    let parent = node.parent()?;
    if parent.kind() == "attribute"
        && parent
            .child_by_field_name("attribute")
            .is_some_and(|a| a.id() == node.id())
    {
        let obj = parent.child_by_field_name("object")?;
        if obj.kind() == "identifier" {
            return obj.utf8_text(src).ok().map(|s| s.to_string());
        }
    }
    None
}

// ---------------------------------------------------------------------------

pub fn parse(ctx: &ParseContext) -> Result<ParseResult> {
    let root = ctx.tree.root_node();
    let parsed_ok = !root.has_error();
    let src = ctx.source;
    let file_path = ctx.file_path;

    let symbols = collect_symbols(root, src, file_path);

    let mut references = Vec::new();
    collect_references(&mut references, root, src);

    // Phase C: scope arena + LEGB resolution
    let flat_syms = crate::parser::flatten_symbols(&symbols);
    let arena = build_scope_arena(root, src, &flat_syms);
    resolve_refs_locally(&arena, &flat_syms, &mut references);

    // Phase C: constructor type tracking
    let mut type_bindings: Vec<PyTypeBinding> = Vec::new();
    collect_py_type_bindings(root, src, &mut type_bindings);
    for r in &mut references {
        if r.context_kind == RefContextKind::Call
            && let Some(recv) = &r.receiver
            && let Some(class_name) = lookup_py_receiver_type(&type_bindings, r.line, recv)
        {
            r.resolved_local_target = Some(format!("{}{}", TYPE_TRACKING_PREFIX, class_name));
        }
    }

    let imports = collect_imports(root, src);

    Ok(ParseResult {
        file_path: file_path.to_string(),
        language: "python".to_string(),
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

fn extract_flags(file_path: &str, name: &str, node: Node, src: &[u8]) -> u32 {
    let mut flags = 0u32;

    if is_test_file(file_path) {
        flags |= FLAG_TEST;
    }

    if name.starts_with("test_") || name.starts_with("Test") {
        flags |= FLAG_TEST;
    }

    if node.kind() == "class_definition" && has_testcase_superclass(node, src) {
        flags |= FLAG_TEST;
    }

    for dec_name in &collect_decorators(node, src) {
        if dec_name == "pytest.fixture" || dec_name == "fixture" {
            flags |= FLAG_TEST;
        }
    }

    flags
}

/// Whether `path` is Python test code for constraint purposes: the pytest and
/// unittest file-naming conventions [`is_test_file`] knows, plus any `test/` or
/// `tests/` directory. The directory forms live here rather than in
/// `is_test_file` deliberately — that one drives symbol `FLAG_TEST`, and a
/// directory says "not production code" without saying "every symbol under it
/// is a test" (sutra/295). Python has no attribute equivalent to `#[cfg(test)]`,
/// so path is the only signal test scope has.
pub fn is_test_path(path: &str) -> bool {
    is_test_file(path) || crate::parser::adapter::path_in_test_dir(path)
}

fn is_test_file(path: &str) -> bool {
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let lower = file_name.to_ascii_lowercase();
    lower.starts_with("test_")
        || lower.ends_with("_test.py")
        || path.contains("/tests/")
        || path.starts_with("tests/")
}

fn has_testcase_superclass(node: Node, src: &[u8]) -> bool {
    let Some(superclasses) = node.child_by_field_name("superclasses") else {
        return false;
    };
    superclasses
        .utf8_text(src)
        .ok()
        .is_some_and(|text| text.contains("TestCase"))
}

// ---------------------------------------------------------------------------
// Symbol extraction
// ---------------------------------------------------------------------------

fn collect_symbols(node: Node, src: &[u8], file_path: &str) -> Vec<ExtractedSymbol> {
    let mut symbols = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_symbol(child, src, file_path, None, &mut symbols);
    }
    symbols
}

fn collect_symbol(
    node: Node,
    src: &[u8],
    file_path: &str,
    class_name: Option<&str>,
    symbols: &mut Vec<ExtractedSymbol>,
) {
    match node.kind() {
        "function_definition" => {
            if let Some(sym) = extract_function(node, src, file_path, class_name) {
                symbols.push(sym);
            }
        }
        "class_definition" => {
            if let Some(sym) = extract_class(node, src, file_path) {
                symbols.push(sym);
            }
        }
        "decorated_definition" => {
            let mut inner = node.walk();
            for child in node.children(&mut inner) {
                match child.kind() {
                    "function_definition" => {
                        if let Some(mut sym) = extract_function(child, src, file_path, class_name) {
                            sym.start_line = node.start_position().row + 1;
                            sym.start_col = node.start_position().column;
                            symbols.push(sym);
                        }
                    }
                    "class_definition" => {
                        if let Some(mut sym) = extract_class(child, src, file_path) {
                            sym.start_line = node.start_position().row + 1;
                            sym.start_col = node.start_position().column;
                            symbols.push(sym);
                        }
                    }
                    _ => {}
                }
            }
        }
        "expression_statement" if class_name.is_none() => {
            let mut inner = node.walk();
            for child in node.children(&mut inner) {
                if child.kind() == "assignment" {
                    collect_assignment_symbols(child, node, src, file_path, symbols);
                }
            }
        }
        _ => {}
    }
}

fn extract_function(
    node: Node,
    src: &[u8],
    file_path: &str,
    class_name: Option<&str>,
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(src).ok()?.to_string();
    let sh = Some(structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    ));

    let (qualified_name, kind) = if let Some(cls) = class_name {
        (format!("{cls}::{name}"), SymbolKind::Method)
    } else {
        (name.clone(), SymbolKind::Function)
    };

    let visibility = if name.starts_with('_') && !name.starts_with("__") {
        Some("private".to_string())
    } else {
        Some("pub".to_string())
    };

    let docstring = extract_docstring_body(node, src);
    let signature = build_fn_signature(node, src);
    let signature_hash = signature
        .as_ref()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
    let language_attrs = extract_fn_language_attrs(node, src);
    let flags = extract_flags(file_path, &name, node, src);

    let (cyclomatic, cognitive, max_nesting) = if let Some(body) = node.child_by_field_name("body")
    {
        (
            Some(complexity::cyclomatic(body, src, "python")),
            Some(complexity::cognitive(body, src, "python")),
            Some(complexity::max_nesting_depth(body, src, "python")),
        )
    } else {
        (Some(1), Some(0), Some(0))
    };

    Some(ExtractedSymbol {
        qualified_name,
        short_name: name,
        kind,
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

fn extract_class(node: Node, src: &[u8], file_path: &str) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = name_node.utf8_text(src).ok()?.to_string();
    let sh = Some(structural_hash::compute(
        node,
        src,
        Some((name_node.start_byte(), name_node.end_byte())),
    ));

    let visibility = if name.starts_with('_') {
        Some("private".to_string())
    } else {
        Some("pub".to_string())
    };

    let docstring = extract_docstring_body(node, src);
    let flags = extract_flags(file_path, &name, node, src);

    let mut children = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            collect_symbol(child, src, file_path, Some(&name), &mut children);
        }
    }

    Some(ExtractedSymbol {
        qualified_name: name.clone(),
        short_name: name,
        kind: SymbolKind::Class,
        signature: None,
        signature_hash: None,
        structural_hash: sh,
        visibility,
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

fn collect_assignment_symbols(
    assign: Node,
    stmt: Node,
    src: &[u8],
    file_path: &str,
    symbols: &mut Vec<ExtractedSymbol>,
) {
    let left = match assign.child_by_field_name("left") {
        Some(n) if n.kind() == "identifier" => n,
        _ => return,
    };

    let name = match left.utf8_text(src) {
        Ok(s) => s.to_string(),
        Err(_) => return,
    };

    let is_all_caps = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        && name.chars().any(|c| c.is_ascii_uppercase());

    let kind = if is_all_caps {
        SymbolKind::Const
    } else {
        SymbolKind::Static
    };

    let docstring = extract_docstring_from_comment(stmt, src);
    let flags = extract_flags(file_path, &name, stmt, src);

    let sh = Some(structural_hash::compute(
        stmt,
        src,
        Some((left.start_byte(), left.end_byte())),
    ));

    symbols.push(ExtractedSymbol {
        qualified_name: name.clone(),
        short_name: name,
        kind,
        signature: None,
        signature_hash: None,
        structural_hash: sh,
        visibility: Some("pub".to_string()),
        start_line: stmt.start_position().row + 1,
        start_col: stmt.start_position().column,
        end_line: stmt.end_position().row + 1,
        end_col: stmt.end_position().column,
        children: vec![],
        parent_symbol_id: None,
        docstring,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags,
        language_attrs: None,
    });
}

// ---------------------------------------------------------------------------
// Docstring extraction
// ---------------------------------------------------------------------------

fn extract_docstring_body(node: Node, src: &[u8]) -> Option<String> {
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    if let Some(child) = body.children(&mut cursor).next()
        && child.kind() == "expression_statement"
    {
        let mut inner = child.walk();
        for gc in child.children(&mut inner) {
            if gc.kind() == "string" {
                let text = gc.utf8_text(src).ok()?;
                return Some(strip_docstring_quotes(text));
            }
        }
    }
    None
}

fn extract_docstring_from_comment(node: Node, src: &[u8]) -> Option<String> {
    let mut doc_lines: Vec<String> = Vec::new();
    let mut sibling = node.prev_sibling();
    while let Some(sib) = sibling {
        if sib.kind() != "comment" {
            break;
        }
        if let Ok(text) = sib.utf8_text(src) {
            let content = text
                .strip_prefix("# ")
                .or_else(|| text.strip_prefix("#"))
                .unwrap_or(text);
            doc_lines.push(content.to_string());
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

fn strip_docstring_quotes(text: &str) -> String {
    let s = text.trim();
    for quote in &["\"\"\"", "'''"] {
        if let Some(inner) = s.strip_prefix(quote).and_then(|i| i.strip_suffix(quote)) {
            return inner.trim().to_string();
        }
    }
    for quote in &["\"", "'"] {
        if let Some(inner) = s.strip_prefix(quote).and_then(|i| i.strip_suffix(quote)) {
            return inner.trim().to_string();
        }
    }
    s.to_string()
}

// ---------------------------------------------------------------------------
// Signature extraction
// ---------------------------------------------------------------------------

fn build_fn_signature(node: Node, src: &[u8]) -> Option<String> {
    let name = node.child_by_field_name("name")?.utf8_text(src).ok()?;
    let params = node
        .child_by_field_name("parameters")?
        .utf8_text(src)
        .ok()?;

    let mut sig = String::new();
    if has_async_keyword(node) {
        sig.push_str("async ");
    }
    sig.push_str("def ");
    sig.push_str(name);
    sig.push_str(params);

    if let Some(ret) = node.child_by_field_name("return_type")
        && let Ok(ret_text) = ret.utf8_text(src)
    {
        sig.push_str(" -> ");
        sig.push_str(ret_text);
    }

    Some(sig)
}

// ---------------------------------------------------------------------------
// Language attributes
// ---------------------------------------------------------------------------

fn extract_fn_language_attrs(node: Node, src: &[u8]) -> Option<String> {
    let mut attrs = serde_json::Map::new();

    if has_async_keyword(node) {
        attrs.insert("is_async".into(), true.into());
    }

    let decorators = collect_decorators(node, src);
    if !decorators.is_empty() {
        attrs.insert("has_decorator".into(), true.into());
        for dec_name in &decorators {
            attrs.insert(format!("decorator:{dec_name}"), true.into());
        }
    }

    if has_type_hints(node) {
        attrs.insert("has_type_hints".into(), true.into());
    }

    if let Some(ret) = node.child_by_field_name("return_type")
        && ret.utf8_text(src).ok() == Some("None")
    {
        attrs.insert("returns_none".into(), true.into());
    }

    if let Some(body) = node.child_by_field_name("body")
        && contains_yield(body)
    {
        attrs.insert("is_generator".into(), true.into());
    }

    Some(serde_json::to_string(&attrs).unwrap_or_else(|_| "{}".into()))
}

fn has_async_keyword(node: Node) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() && child.kind() == "async" {
            return true;
        }
        if child.is_named() || child.kind() == "def" {
            break;
        }
    }
    false
}

fn collect_decorators(node: Node, src: &[u8]) -> Vec<String> {
    let mut decorators = Vec::new();
    let Some(parent) = node.parent() else {
        return decorators;
    };
    if parent.kind() != "decorated_definition" {
        return decorators;
    }

    let mut cursor = parent.walk();
    for child in parent.children(&mut cursor) {
        if child.kind() != "decorator" {
            continue;
        }
        let mut dec_cursor = child.walk();
        for dec_child in child.children(&mut dec_cursor) {
            if !dec_child.is_named() {
                continue;
            }
            match dec_child.kind() {
                "identifier" | "attribute" => {
                    if let Ok(name) = dec_child.utf8_text(src) {
                        decorators.push(name.to_string());
                    }
                }
                "call" => {
                    if let Some(func) = dec_child.child_by_field_name("function")
                        && let Ok(name) = func.utf8_text(src)
                    {
                        decorators.push(name.to_string());
                    }
                }
                _ => {}
            }
            break;
        }
    }

    decorators
}

fn has_type_hints(node: Node) -> bool {
    if node.child_by_field_name("return_type").is_some() {
        return true;
    }

    if let Some(params) = node.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for child in params.children(&mut cursor) {
            if matches!(child.kind(), "typed_parameter" | "typed_default_parameter") {
                return true;
            }
        }
    }

    false
}

fn contains_yield(node: Node) -> bool {
    if node.kind() == "yield" {
        return true;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if matches!(
            child.kind(),
            "function_definition" | "class_definition" | "lambda"
        ) {
            continue;
        }
        if contains_yield(child) {
            return true;
        }
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

    if node.kind() == "identifier"
        && !is_definition_name(node)
        && let Ok(name) = node.utf8_text(src)
    {
        let ctx = classify_ref_context(node);
        if ctx != RefContextKind::Other {
            let receiver = if ctx == RefContextKind::Call {
                extract_py_call_receiver(node, src)
            } else {
                None
            };
            refs.push(ExtractedRef {
                name: name.to_string(),
                line: node.start_position().row + 1,
                col: node.start_position().column,
                context_kind: ctx,
                resolved_local_target: None,
                receiver,
            });
        }
        if ctx == RefContextKind::Call
            && let Some(chain) = build_dotted_call_chain(node, src)
        {
            refs.push(ExtractedRef {
                name: chain,
                line: node.start_position().row + 1,
                col: node.start_position().column,
                context_kind: RefContextKind::Call,
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

/// For `requests.get()`, the identifier `get` is the attribute field of an
/// `attribute` node whose parent is `call`. Walk up through nested `attribute`
/// nodes collecting the object segments to produce `"requests.get"`.
fn build_dotted_call_chain(node: Node, src: &[u8]) -> Option<String> {
    let attr_node = node.parent()?;
    if attr_node.kind() != "attribute" {
        return None;
    }
    if attr_node.child_by_field_name("attribute").map(|a| a.id()) != Some(node.id()) {
        return None;
    }

    let mut segments = vec![node.utf8_text(src).ok()?];
    let mut current = attr_node;
    loop {
        let object = current.child_by_field_name("object")?;
        match object.kind() {
            "identifier" => {
                segments.push(object.utf8_text(src).ok()?);
                break;
            }
            "attribute" => {
                let attr_part = object.child_by_field_name("attribute")?;
                segments.push(attr_part.utf8_text(src).ok()?);
                current = object;
            }
            _ => return None,
        }
    }
    if segments.len() < 2 {
        return None;
    }
    segments.reverse();
    Some(segments.join("."))
}

fn is_definition_name(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        "function_definition" | "class_definition" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        "assignment" | "augmented_assignment" => parent
            .child_by_field_name("left")
            .is_some_and(|n| n.id() == node.id()),
        "for_statement" => parent
            .child_by_field_name("left")
            .is_some_and(|n| n.id() == node.id()),
        "parameter" | "typed_parameter" | "typed_default_parameter" | "default_parameter" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        "as_pattern" => parent
            .child_by_field_name("alias")
            .is_some_and(|n| n.id() == node.id()),
        "keyword_argument" => parent
            .child_by_field_name("name")
            .is_some_and(|n| n.id() == node.id()),
        "aliased_import" => true,
        "dotted_name" => parent.parent().is_some_and(|gp| {
            matches!(
                gp.kind(),
                "import_statement" | "import_from_statement" | "aliased_import"
            )
        }),
        "global_statement" | "nonlocal_statement" => true,
        _ => false,
    }
}

fn classify_ref_context(node: Node) -> RefContextKind {
    let Some(parent) = node.parent() else {
        return RefContextKind::Other;
    };

    // Call detection takes highest priority
    match parent.kind() {
        "call" => {
            if parent
                .child_by_field_name("function")
                .is_some_and(|f| f.id() == node.id())
            {
                return RefContextKind::Call;
            }
        }
        "attribute"
            if parent
                .child_by_field_name("attribute")
                .is_some_and(|a| a.id() == node.id())
                && parent.parent().is_some_and(|gp| {
                    gp.kind() == "call"
                        && gp
                            .child_by_field_name("function")
                            .is_some_and(|f| f.id() == parent.id())
                }) =>
        {
            return RefContextKind::Call;
        }
        _ => {}
    }

    // Any ancestor being a "type" node means we're inside a type annotation
    if has_type_ancestor(node) {
        return RefContextKind::TypeUse;
    }

    if parent.kind() == "attribute"
        && parent
            .child_by_field_name("attribute")
            .is_some_and(|a| a.id() == node.id())
    {
        return RefContextKind::FieldAccess;
    }

    RefContextKind::Other
}

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

fn collect_imports(node: Node, src: &[u8]) -> Vec<ExtractedImport> {
    let mut imports = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "import_statement" => collect_import_names(child, src, &mut imports),
            "import_from_statement" => collect_import_from(child, src, &mut imports),
            _ => {}
        }
    }
    imports
}

fn collect_import_names(node: Node, src: &[u8], imports: &mut Vec<ExtractedImport>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "dotted_name" => {
                if let Ok(path) = child.utf8_text(src) {
                    imports.push(ExtractedImport {
                        raw_path: path.to_string(),
                        line: child.start_position().row + 1,
                        kind: "import",
                        alias: None,
                        is_test: false,
                    });
                }
            }
            "aliased_import" => {
                if let Some(name) = child.child_by_field_name("name")
                    && let Ok(path) = name.utf8_text(src)
                {
                    let alias = child
                        .child_by_field_name("alias")
                        .and_then(|a| a.utf8_text(src).ok())
                        .map(|s| s.to_string());
                    imports.push(ExtractedImport {
                        raw_path: path.to_string(),
                        line: name.start_position().row + 1,
                        kind: "import",
                        alias,
                        is_test: false,
                    });
                }
            }
            _ => {}
        }
    }
}

fn collect_import_from(node: Node, src: &[u8], imports: &mut Vec<ExtractedImport>) {
    let module_node = node.child_by_field_name("module_name");
    let prefix = module_node
        .and_then(|m| m.utf8_text(src).ok())
        .unwrap_or("");
    let module_id = module_node.map(|m| m.id());
    let line = node.start_position().row + 1;
    let mut found_name = false;
    let mut cursor = node.walk();

    for child in node.children(&mut cursor) {
        if module_id.is_some_and(|id| id == child.id()) {
            continue;
        }
        let (name_text, alias_text) = match child.kind() {
            "dotted_name" => (child.utf8_text(src).ok(), None),
            "aliased_import" => (
                child
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(src).ok()),
                child
                    .child_by_field_name("alias")
                    .and_then(|a| a.utf8_text(src).ok()),
            ),
            _ => continue,
        };
        if let Some(name) = name_text {
            found_name = true;
            let raw_path = if prefix.is_empty() {
                name.to_string()
            } else if prefix.bytes().all(|b| b == b'.') {
                format!("{prefix}{name}")
            } else {
                format!("{prefix}.{name}")
            };
            imports.push(ExtractedImport {
                raw_path,
                line,
                kind: "from_import",
                alias: alias_text.map(|s| s.to_string()),
                is_test: false,
            });
        }
    }

    // Wildcard: `from module import *`
    if !found_name && !prefix.is_empty() {
        imports.push(ExtractedImport {
            raw_path: prefix.to_string(),
            line,
            kind: "from_import",
            alias: None,
            is_test: false,
        });
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::adapter::{LanguageAdapter, ParserPool};
    use std::time::Duration;

    struct PythonTestAdapter;
    impl LanguageAdapter for PythonTestAdapter {
        fn language_id(&self) -> &str {
            "python"
        }
        fn extensions(&self) -> &[&str] {
            &["py"]
        }
        fn grammar(&self) -> tree_sitter::Language {
            tree_sitter_python::LANGUAGE.into()
        }
        fn parse(&self, ctx: &ParseContext) -> Result<ParseResult> {
            super::parse(ctx)
        }
    }

    fn parse_py(code: &str) -> ParseResult {
        let mut pool = ParserPool::new(Duration::from_secs(5));
        pool.parse_with(&PythonTestAdapter, code, "test.py")
            .unwrap()
    }

    fn find_sym<'a>(result: &'a ParseResult, name: &str) -> &'a ExtractedSymbol {
        crate::parser::flatten_symbols(&result.symbols)
            .into_iter()
            .find(|s| s.short_name == name)
            .unwrap_or_else(|| panic!("symbol {name} not found"))
    }

    #[test]
    fn smoke_function_extraction() {
        let r = parse_py("def greet(name):\n    return f'hello {name}'\n");
        assert_eq!(r.symbols.len(), 1);
        let sym = &r.symbols[0];
        assert_eq!(sym.short_name, "greet");
        assert_eq!(sym.kind, SymbolKind::Function);
        assert_eq!(sym.visibility.as_deref(), Some("pub"));
    }

    #[test]
    fn class_with_methods() {
        let r = parse_py(
            "class Greeter:\n    def hello(self):\n        pass\n    def bye(self):\n        pass\n",
        );
        assert_eq!(r.symbols.len(), 1);
        let cls = &r.symbols[0];
        assert_eq!(cls.short_name, "Greeter");
        assert_eq!(cls.kind, SymbolKind::Class);
        assert_eq!(cls.children.len(), 2);
        assert_eq!(cls.children[0].qualified_name, "Greeter::hello");
        assert_eq!(cls.children[0].kind, SymbolKind::Method);
        assert_eq!(cls.children[1].qualified_name, "Greeter::bye");
    }

    #[test]
    fn decorated_function_attrs() {
        let r = parse_py("@app.route(\"/\")\ndef index():\n    pass\n");
        let sym = find_sym(&r, "index");
        let la: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(sym.language_attrs.as_deref().unwrap()).unwrap();
        assert_eq!(
            la.get("has_decorator").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            la.get("decorator:app.route").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn async_function() {
        let r = parse_py("async def fetch(url):\n    pass\n");
        let sym = find_sym(&r, "fetch");
        let la: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(sym.language_attrs.as_deref().unwrap()).unwrap();
        assert_eq!(la.get("is_async").and_then(|v| v.as_bool()), Some(true));
        assert!(sym.signature.as_deref().unwrap().starts_with("async def"));
    }

    #[test]
    fn module_level_assignments() {
        let r = parse_py("MAX_RETRIES = 3\ndefault_name = 'world'\n");
        assert_eq!(r.symbols.len(), 2);
        let max_r = find_sym(&r, "MAX_RETRIES");
        assert_eq!(max_r.kind, SymbolKind::Const);
        let default = find_sym(&r, "default_name");
        assert_eq!(default.kind, SymbolKind::Static);
    }

    #[test]
    fn import_extraction() {
        let r = parse_py("import os\nimport sys\nfrom pathlib import Path\n");
        assert_eq!(r.imports.len(), 3);
        assert_eq!(r.imports[0].raw_path, "os");
        assert_eq!(r.imports[1].raw_path, "sys");
        assert_eq!(r.imports[2].raw_path, "pathlib.Path");
    }

    #[test]
    fn relative_import() {
        let r = parse_py("from . import utils\nfrom ..models import User\n");
        assert_eq!(r.imports.len(), 2);
        assert_eq!(r.imports[0].raw_path, ".utils");
        assert_eq!(r.imports[1].raw_path, "..models.User");
    }

    #[test]
    fn from_import_multi_name() {
        let r = parse_py("from os.path import join, exists\n");
        assert_eq!(r.imports.len(), 2);
        assert_eq!(r.imports[0].raw_path, "os.path.join");
        assert_eq!(r.imports[1].raw_path, "os.path.exists");
    }

    #[test]
    fn from_import_aliased() {
        let r = parse_py("from collections import OrderedDict as OD\n");
        assert_eq!(r.imports.len(), 1);
        assert_eq!(r.imports[0].raw_path, "collections.OrderedDict");
        assert_eq!(r.imports[0].alias.as_deref(), Some("OD"));
    }

    #[test]
    fn from_import_wildcard() {
        let r = parse_py("from os.path import *\n");
        assert_eq!(r.imports.len(), 1);
        assert_eq!(r.imports[0].raw_path, "os.path");
    }

    #[test]
    fn type_use_generic_annotation() {
        let r = parse_py("def foo(x: list[User]) -> None:\n    pass\n");
        let type_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(type_refs.iter().any(|r| r.name == "list"));
        assert!(type_refs.iter().any(|r| r.name == "User"));
    }

    #[test]
    fn type_use_qualified_annotation() {
        let r = parse_py("def foo(x: pkg.MyType) -> None:\n    pass\n");
        let type_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(type_refs.iter().any(|r| r.name == "pkg"));
        assert!(type_refs.iter().any(|r| r.name == "MyType"));
    }

    #[test]
    fn call_reference() {
        let r = parse_py("x = foo()\n");
        let call_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::Call)
            .collect();
        assert!(call_refs.iter().any(|r| r.name == "foo"));
    }

    #[test]
    fn method_call_reference() {
        let r = parse_py("x = obj.method()\n");
        let call_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::Call)
            .collect();
        assert!(call_refs.iter().any(|r| r.name == "method"));
    }

    #[test]
    fn field_access_reference() {
        let r = parse_py("x = obj.attr\n");
        let fa_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::FieldAccess)
            .collect();
        assert!(fa_refs.iter().any(|r| r.name == "attr"));
    }

    #[test]
    fn type_use_reference() {
        let r = parse_py("def foo(x: int) -> str:\n    pass\n");
        let type_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(type_refs.iter().any(|r| r.name == "int"));
        assert!(type_refs.iter().any(|r| r.name == "str"));
    }

    #[test]
    fn test_flag_prefix() {
        let r = parse_py("def test_something():\n    pass\n");
        let sym = find_sym(&r, "test_something");
        assert_ne!(sym.flags & FLAG_TEST, 0);
    }

    #[test]
    fn test_flag_file() {
        let code = "def helper():\n    pass\n";
        let mut pool = ParserPool::new(Duration::from_secs(5));
        let r = pool
            .parse_with(&PythonTestAdapter, code, "tests/test_foo.py")
            .unwrap();
        let sym = find_sym(&r, "helper");
        assert_ne!(sym.flags & FLAG_TEST, 0);
    }

    #[test]
    fn generator_detection() {
        let r = parse_py("def gen():\n    yield 1\n    yield 2\n");
        let sym = find_sym(&r, "gen");
        let la: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(sym.language_attrs.as_deref().unwrap()).unwrap();
        assert_eq!(la.get("is_generator").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn type_hint_detection() {
        let r = parse_py("def add(a: int, b: int) -> int:\n    return a + b\n");
        let sym = find_sym(&r, "add");
        let la: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(sym.language_attrs.as_deref().unwrap()).unwrap();
        assert_eq!(
            la.get("has_type_hints").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(sym.signature.as_deref().unwrap().contains("-> int"));
    }

    #[test]
    fn docstring_extracted() {
        let r = parse_py("def foo():\n    \"\"\"This is a docstring.\"\"\"\n    pass\n");
        let sym = find_sym(&r, "foo");
        assert_eq!(sym.docstring.as_deref(), Some("This is a docstring."));
    }

    #[test]
    fn private_function() {
        let r = parse_py("def _internal():\n    pass\n");
        let sym = find_sym(&r, "_internal");
        assert_eq!(sym.visibility.as_deref(), Some("private"));
    }

    #[test]
    fn complexity_computed() {
        let r = parse_py(
            "def foo(x):\n    if x > 0:\n        return 1\n    else:\n        return -1\n",
        );
        let sym = find_sym(&r, "foo");
        assert!(sym.cyclomatic.unwrap() >= 2);
    }

    #[test]
    fn returns_none_detected() {
        let r = parse_py("def cleanup() -> None:\n    pass\n");
        let sym = find_sym(&r, "cleanup");
        let la: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(sym.language_attrs.as_deref().unwrap()).unwrap();
        assert_eq!(la.get("returns_none").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn decorated_definition_span_includes_decorator() {
        let code = "@decorator\ndef foo():\n    pass\n";
        let r = parse_py(code);
        let sym = find_sym(&r, "foo");
        assert_eq!(sym.start_line, 1);
    }

    #[test]
    fn dotted_call_emits_chain_ref() {
        let r = parse_py("x = requests.get(url)\n");
        let call_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::Call)
            .collect();
        assert!(call_refs.iter().any(|r| r.name == "get"));
        assert!(call_refs.iter().any(|r| r.name == "requests.get"));
    }

    #[test]
    fn deep_dotted_call_emits_chain_ref() {
        let r = parse_py("x = a.b.c()\n");
        let call_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::Call)
            .collect();
        assert!(call_refs.iter().any(|r| r.name == "c"));
        assert!(call_refs.iter().any(|r| r.name == "a.b.c"));
    }

    #[test]
    fn simple_call_no_spurious_chain() {
        let r = parse_py("x = foo()\n");
        let call_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.context_kind == RefContextKind::Call)
            .collect();
        assert_eq!(call_refs.len(), 1);
        assert_eq!(call_refs[0].name, "foo");
    }

    #[test]
    fn import_kind_plain_import() {
        let r = parse_py("import os\nimport pkg.sub\n");
        assert!(r.imports.iter().all(|i| i.kind == "import"));
    }

    #[test]
    fn import_kind_from_import() {
        let r = parse_py("from os.path import join\nfrom . import utils\n");
        assert!(r.imports.iter().all(|i| i.kind == "from_import"));
    }

    // -----------------------------------------------------------------------
    // Phase C: LEGB scope resolution tests
    // -----------------------------------------------------------------------

    #[test]
    fn scope_local_binding_shadows_call() {
        let code = "\
def foo():
    handler = get_handler()
    handler()
";
        let r = parse_py(code);
        let refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "handler" && r.line == 3)
            .collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].resolved_local_target.as_deref(),
            Some(LOCAL_BINDING_SENTINEL)
        );
    }

    #[test]
    fn scope_param_binding_used_as_call() {
        let code = "\
def greet(formatter):
    formatter()
";
        let r = parse_py(code);
        let refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "formatter" && r.line == 2)
            .collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].resolved_local_target.as_deref(),
            Some(LOCAL_BINDING_SENTINEL)
        );
    }

    #[test]
    fn scope_class_not_visible_as_call_in_method() {
        let code = "\
class Foo:
    handler = None
    def method(self):
        handler()
";
        let r = parse_py(code);
        let refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "handler" && r.line == 4)
            .collect();
        assert!(!refs.is_empty(), "handler() should emit a Call ref");
        assert!(
            refs[0].resolved_local_target.is_none(),
            "bare `handler` in method should NOT resolve through class scope"
        );
    }

    #[test]
    fn scope_module_level_function_resolves() {
        let code = "\
def helper():
    pass

def main():
    helper()
";
        let r = parse_py(code);
        let helper_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "helper" && r.line == 5)
            .collect();
        assert_eq!(helper_refs.len(), 1);
        assert_eq!(
            helper_refs[0].resolved_local_target.as_deref(),
            Some("helper")
        );
    }

    #[test]
    fn scope_nested_function_call_from_enclosing() {
        let code = "\
def outer():
    callback = make_callback()
    def inner():
        callback()
";
        let r = parse_py(code);
        let refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "callback" && r.line == 4)
            .collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].resolved_local_target.as_deref(),
            Some(LOCAL_BINDING_SENTINEL),
            "enclosing scope binding should resolve (LEGB 'E')"
        );
    }

    #[test]
    fn scope_for_loop_target_binding() {
        let code = "\
def process():
    for item in items:
        item()
";
        let r = parse_py(code);
        let item_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "item" && r.line == 3)
            .collect();
        assert_eq!(item_refs.len(), 1);
        assert_eq!(
            item_refs[0].resolved_local_target.as_deref(),
            Some(LOCAL_BINDING_SENTINEL)
        );
    }

    #[test]
    fn scope_decorated_function_resolves() {
        let code = "\
@app.route('/')
def index():
    pass

def main():
    index()
";
        let r = parse_py(code);
        let refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "index" && r.line == 6)
            .collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].resolved_local_target.as_deref(),
            Some("index"),
            "decorated function should still resolve via scope chain"
        );
    }

    #[test]
    fn scope_binding_inside_if_block() {
        let code = "\
def process():
    if True:
        handler = get_handler()
    handler()
";
        let r = parse_py(code);
        let refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "handler" && r.line == 4)
            .collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].resolved_local_target.as_deref(),
            Some(LOCAL_BINDING_SENTINEL),
            "assignment inside if block is function-wide in Python"
        );
    }

    #[test]
    fn scope_binding_inside_while_block() {
        let code = "\
def run():
    while True:
        conn = connect()
    conn()
";
        let r = parse_py(code);
        let refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "conn" && r.line == 4)
            .collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(
            refs[0].resolved_local_target.as_deref(),
            Some(LOCAL_BINDING_SENTINEL),
            "assignment inside while block is function-wide in Python"
        );
    }

    #[test]
    fn type_tracking_class_body_not_leaked() {
        let code = "\
class Holder:
    c = Cache()

def f():
    c.get()
";
        let r = parse_py(code);
        let refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "get" && r.context_kind == RefContextKind::Call && r.line == 5)
            .collect();
        assert!(!refs.is_empty());
        assert!(
            refs[0].resolved_local_target.is_none(),
            "class-body assignment should NOT create type binding for outer function"
        );
    }

    // -----------------------------------------------------------------------
    // Phase C: import alias tests
    // -----------------------------------------------------------------------

    #[test]
    fn import_alias_captured() {
        let r = parse_py("import numpy as np\n");
        assert_eq!(r.imports.len(), 1);
        assert_eq!(r.imports[0].raw_path, "numpy");
        assert_eq!(r.imports[0].alias.as_deref(), Some("np"));
    }

    #[test]
    fn from_import_alias_captured() {
        let r = parse_py("from datetime import datetime as dt\n");
        assert_eq!(r.imports.len(), 1);
        assert_eq!(r.imports[0].raw_path, "datetime.datetime");
        assert_eq!(r.imports[0].alias.as_deref(), Some("dt"));
    }

    #[test]
    fn unaliased_import_has_no_alias() {
        let r = parse_py("import os\nfrom sys import argv\n");
        assert!(r.imports.iter().all(|i| i.alias.is_none()));
    }

    // -----------------------------------------------------------------------
    // Phase C: constructor type tracking tests
    // -----------------------------------------------------------------------

    #[test]
    fn constructor_type_tracking_sets_hint() {
        let code = "\
def main():
    c = Client()
    c.get()
";
        let r = parse_py(code);
        let get_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "get" && r.context_kind == RefContextKind::Call)
            .collect();
        assert!(!get_refs.is_empty(), "should have a call ref for 'get'");
        let get_ref = &get_refs[0];
        assert_eq!(get_ref.receiver.as_deref(), Some("c"));
        assert_eq!(
            get_ref.resolved_local_target.as_deref(),
            Some("::type_tracking::Client"),
            "constructor type tracking should set TYPE_TRACKING_PREFIX hint"
        );
    }

    #[test]
    fn constructor_type_tracking_lowercase_not_constructor() {
        let code = "\
def main():
    x = factory()
    x.run()
";
        let r = parse_py(code);
        let run_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "run" && r.context_kind == RefContextKind::Call)
            .collect();
        assert!(!run_refs.is_empty());
        assert!(
            run_refs[0].resolved_local_target.is_none(),
            "lowercase factory() should NOT set type tracking hint"
        );
    }

    #[test]
    fn receiver_captured_for_method_call() {
        let code = "x.method()\n";
        let r = parse_py(code);
        let method_refs: Vec<_> = r
            .references
            .iter()
            .filter(|r| r.name == "method" && r.context_kind == RefContextKind::Call)
            .collect();
        assert_eq!(method_refs.len(), 1);
        assert_eq!(method_refs[0].receiver.as_deref(), Some("x"));
    }
}
