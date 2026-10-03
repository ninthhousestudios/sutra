//! C++ adapter. Layered on `c.rs` the way `typescript.rs` layers on
//! `javascript.rs`: the C subset (functions, structs, enums, typedefs, macros,
//! globals, includes, references) goes through the C extractors with
//! [`Dialect::Cpp`], so the two languages cannot drift apart on shared syntax.
//! Design decisions: `docs/adding-languages.md` § C++ (sutra/525).

use crate::error::Result;
use crate::parser::adapter::{ParseContext, node_text};
use crate::parser::c::{self, Dialect};
use crate::parser::{ExtractedSymbol, ParseResult, SymbolKind, complexity, structural_hash};
use tree_sitter::Node;

/// Extensions the C++ adapter owns outright. `.h` and `.c` are shared with
/// the C adapter and routed per workspace (sutra/525), so they are not here.
/// C++20 module units (`cppm`, `ixx`) are deliberately not indexed.
pub(super) const EXTENSIONS: &[&str] = &[
    "cc", "cpp", "cxx", "c++", "hpp", "hh", "hxx", "h++", "ipp", "tpp", "inl",
];

pub fn parse(ctx: &ParseContext) -> Result<ParseResult> {
    c::parse_dialect(ctx, Dialect::Cpp)
}

/// Whether `path` is C++ test code: gtest's `*_test.cc` / `*_unittest.cc`
/// naming (any C++ extension, plus `.c` for a cpp-only workspace), the shared
/// `test_*` prefix, and `test/`/`tests/` directories.
pub fn is_test_path(path: &str) -> bool {
    c::is_test_path_for(path, Dialect::Cpp)
}

/// The C++ half of [`Dialect`]'s test-suffix check, on a lowercased file name.
pub(super) fn has_test_suffix(lower_file_name: &str) -> bool {
    let Some((stem, ext)) = lower_file_name.rsplit_once('.') else {
        return false;
    };
    (stem.ends_with("_test") || stem.ends_with("_unittest"))
        && (ext == "c" || EXTENSIONS.contains(&ext))
}

// ---------------------------------------------------------------------------
// Scoped symbol extraction
// ---------------------------------------------------------------------------
//
// Every extractor names its symbols relative to its own scope (`Foo::bar`
// inside class `Foo`), and each enclosing namespace or class prefixes its
// children with its own name on the way out. A nested symbol therefore ends
// up `ns::Outer::Inner::member` without a name-context stack threaded through
// the C extractors.

/// Top-level symbols of a C++ translation unit.
pub(super) fn collect_symbols(root: Node, src: &[u8], file_path: &str) -> Vec<ExtractedSymbol> {
    let mut symbols = Vec::new();
    collect_children(root, src, file_path, &mut symbols);
    symbols
}

fn collect_children(node: Node, src: &[u8], file_path: &str, out: &mut Vec<ExtractedSymbol>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_node(child, src, file_path, out);
    }
}

/// One namespace-scope node. Anything without C++-specific handling goes to
/// the shared C dispatcher.
fn collect_node(node: Node, src: &[u8], file_path: &str, out: &mut Vec<ExtractedSymbol>) {
    match node.kind() {
        "namespace_definition" => collect_namespace(node, src, file_path, out),
        "alias_declaration" => out.extend(extract_alias(node, src)),
        // `extern "C" { ... }` and `extern "C" int f();` add no scope.
        "linkage_specification" => {
            if let Some(body) = node.child_by_field_name("body") {
                if body.kind() == "declaration_list" {
                    collect_children(body, src, file_path, out);
                } else {
                    collect_node(body, src, file_path, out);
                }
            }
        }
        // Include guards wrap whole headers; conditional blocks add no scope.
        kind if is_preproc_block(kind) => collect_children(node, src, file_path, out),
        // Templates are unwrapped by the template pass (sutra/529).
        "template_declaration" => {}
        _ => c::collect_symbol(node, src, file_path, Dialect::Cpp, out),
    }
}

/// `namespace a { }` → Module `a`; `namespace a::b { }` → one Module `a::b`.
/// A reopened namespace is another Module with the same qualified name. An
/// anonymous namespace emits no symbol: its members join the enclosing scope
/// as private, since nothing outside the translation unit can name them.
fn collect_namespace(node: Node, src: &[u8], file_path: &str, out: &mut Vec<ExtractedSymbol>) {
    let mut children = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        collect_children(body, src, file_path, &mut children);
    }
    let Some(name_node) = node.child_by_field_name("name") else {
        for child in &mut children {
            make_private(child);
        }
        out.append(&mut children);
        return;
    };
    let qualified = node_text(name_node, src)
        .split_whitespace()
        .collect::<String>();
    for child in &mut children {
        prefix_qualified(child, &qualified);
    }
    let mut attrs = serde_json::Map::new();
    if node.child(0).is_some_and(|n| n.kind() == "inline") {
        attrs.insert("is_inline".into(), true.into());
    }
    let short_name = qualified.rsplit("::").next().unwrap_or(&qualified);
    let mut sym = base_symbol(node, src, Some(name_node), short_name, SymbolKind::Module);
    sym.qualified_name = qualified;
    sym.docstring = c::extract_docstring(node, src);
    sym.visibility = Some("pub".to_string());
    sym.children = children;
    sym.language_attrs = attrs_json(attrs);
    out.push(sym);
}

/// A bodied `class` / `struct` / `union` → Struct, with its members as
/// children. `language_attrs.class_key` records which keyword declared it.
pub(super) fn extract_class(
    node: Node,
    src: &[u8],
    file_path: &str,
    doc_anchor: Option<Node>,
) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, src);
    let class_key = match node.kind() {
        "class_specifier" => "class",
        "union_specifier" => "union",
        _ => "struct",
    };
    let mut members = Vec::new();
    if let Some(body) = node.child_by_field_name("body") {
        let mut access = if class_key == "class" {
            Access::Private
        } else {
            Access::Public
        };
        collect_members(body, src, file_path, &mut access, &mut members);
    }
    for member in &mut members {
        prefix_qualified(member, name);
    }
    let mut attrs = serde_json::Map::new();
    attrs.insert("class_key".into(), class_key.into());

    let mut sym = base_symbol(node, src, Some(name_node), name, SymbolKind::Struct);
    sym.docstring = c::extract_docstring(doc_anchor.unwrap_or(node), src);
    sym.visibility = Some("pub".to_string());
    sym.children = members;
    sym.language_attrs = attrs_json(attrs);
    Some(sym)
}

/// Add enumerators to an enum the C extractor built, as Const children
/// `E::A`. Unscoped enumerators also leak into the enclosing scope, but C++11
/// accepts `E::A` for both forms, so one qualified name covers each.
pub(super) fn with_enumerators(
    mut sym: ExtractedSymbol,
    node: Node,
    src: &[u8],
) -> ExtractedSymbol {
    let scoped = has_token(node, "class") || has_token(node, "struct");
    if let Some(body) = node.child_by_field_name("body") {
        let mut cursor = body.walk();
        for e in body.children(&mut cursor) {
            if e.kind() != "enumerator" {
                continue;
            }
            let Some(name_node) = e.child_by_field_name("name") else {
                continue;
            };
            let name = node_text(name_node, src);
            let mut child = base_symbol(e, src, Some(name_node), name, SymbolKind::Const);
            prefix_qualified(&mut child, &sym.qualified_name);
            child.visibility = Some("pub".to_string());
            child.docstring = c::extract_docstring(e, src);
            sym.children.push(child);
        }
    }
    if scoped {
        let mut attrs = serde_json::Map::new();
        attrs.insert("is_scoped".into(), true.into());
        sym.language_attrs = attrs_json(attrs);
    }
    sym
}

/// The access section a class member falls in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Public,
    Protected,
    Private,
}

impl Access {
    fn parse(text: &str) -> Option<Self> {
        match text.trim() {
            "public" => Some(Self::Public),
            "protected" => Some(Self::Protected),
            "private" => Some(Self::Private),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Protected => "protected",
            Self::Private => "private",
        }
    }

    /// sutra's two-valued visibility: only `public` members are API surface.
    fn visibility(self) -> &'static str {
        match self {
            Self::Public => "pub",
            Self::Protected | Self::Private => "private",
        }
    }
}

/// Walk a `field_declaration_list`. `access` carries across `access_specifier`
/// labels and through preprocessor blocks, which add no scope.
fn collect_members(
    body: Node,
    src: &[u8],
    file_path: &str,
    access: &mut Access,
    out: &mut Vec<ExtractedSymbol>,
) {
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        let start = out.len();
        match child.kind() {
            "access_specifier" => {
                if let Some(a) = Access::parse(node_text(child, src)) {
                    *access = a;
                }
            }
            "field_declaration" => {
                if let Some(type_node) = child.child_by_field_name("type")
                    && type_node.child_by_field_name("body").is_some()
                    && let Some(sym) = c::extract_type_specifier(
                        type_node,
                        src,
                        file_path,
                        Dialect::Cpp,
                        Some(child),
                    )
                {
                    out.push(sym);
                }
                for declarator in c::field_children(child, "declarator") {
                    if is_function_like(declarator) {
                        out.extend(extract_method(child, declarator, src, file_path));
                    } else {
                        out.extend(extract_data_member(child, declarator, src));
                    }
                }
            }
            // Constructor and destructor declarations have no type, so they
            // parse as `declaration` rather than `field_declaration`.
            "declaration" => {
                for declarator in c::field_children(child, "declarator") {
                    if is_function_like(declarator) {
                        out.extend(extract_method(child, declarator, src, file_path));
                    }
                }
            }
            "function_definition" => {
                if let Some(declarator) = child.child_by_field_name("declarator") {
                    out.extend(extract_method(child, declarator, src, file_path));
                }
            }
            "alias_declaration" => out.extend(extract_alias(child, src)),
            "type_definition" => c::collect_symbol(child, src, file_path, Dialect::Cpp, out),
            // Members inside set their own access; the block adds no scope.
            kind if is_preproc_block(kind) => {
                collect_members(child, src, file_path, access, out);
                continue;
            }
            _ => {}
        }
        for sym in &mut out[start..] {
            set_access(sym, *access);
        }
    }
}

fn is_preproc_block(kind: &str) -> bool {
    matches!(
        kind,
        "preproc_ifdef" | "preproc_if" | "preproc_else" | "preproc_elif" | "preproc_elifdef"
    )
}

fn is_function_like(declarator: Node) -> bool {
    declarator.kind() == "operator_cast" || c::find_function_declarator(declarator).is_some()
}

/// A member function — declared, defined in-class, or `= default`/`= delete`.
/// A bodiless declaration carries `declaration_only` and no complexity, so it
/// does not show up as a complexity-0 twin of its definition (sutra/525).
fn extract_method(
    node: Node,
    declarator: Node,
    src: &[u8],
    file_path: &str,
) -> Option<ExtractedSymbol> {
    let (name, name_node) = method_name(declarator, src)?;
    let body = node.child_by_field_name("body");
    let mut cursor = node.walk();
    let clause = node
        .named_children(&mut cursor)
        .find(|n| matches!(n.kind(), "default_method_clause" | "delete_method_clause"));

    let mut attrs = c::fn_language_attrs(node, src, declarator);
    if body.is_none() && clause.is_none() {
        attrs.insert("declaration_only".into(), true.into());
    }
    match clause.map(|n| n.kind()) {
        Some("default_method_clause") => {
            attrs.insert("is_defaulted".into(), true.into());
        }
        Some("delete_method_clause") => {
            attrs.insert("is_deleted".into(), true.into());
        }
        _ => {}
    }
    if has_token(node, "virtual") {
        attrs.insert("is_virtual".into(), true.into());
    }
    if node.kind() == "field_declaration"
        && node
            .child_by_field_name("default_value")
            .is_some_and(|v| node_text(v, src).trim() == "0")
    {
        attrs.insert("is_pure_virtual".into(), true.into());
    }

    let sig_end = body.or(clause).map_or(node.end_byte(), |n| n.start_byte());
    let signature = node_text(node, src)
        .get(..sig_end - node.start_byte())
        .map(|s| s.trim().trim_end_matches(';').trim_end().to_string());

    let mut sym = base_symbol(node, src, Some(name_node), &name, SymbolKind::Method);
    sym.signature_hash = signature
        .as_ref()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
    sym.signature = signature;
    sym.docstring = c::extract_docstring(node, src);
    sym.flags = c::extract_flags(file_path, &sym.short_name, node, Dialect::Cpp);
    match body {
        Some(body) => {
            sym.cyclomatic = Some(complexity::cyclomatic(
                body,
                src,
                Dialect::Cpp.language_id(),
            ));
            sym.cognitive = Some(complexity::cognitive(body, src, Dialect::Cpp.language_id()));
        }
        // `= default` / `= delete` define the function with no body to score.
        None if clause.is_some() => {
            sym.cyclomatic = Some(1);
            sym.cognitive = Some(0);
        }
        None => {}
    }
    sym.language_attrs = attrs_json(attrs);
    Some(sym)
}

/// A member function's stable name and the node it is hashed around:
/// `get`, `~Foo`, `operator==`, `operator bool`.
fn method_name<'a>(declarator: Node<'a>, src: &[u8]) -> Option<(String, Node<'a>)> {
    if declarator.kind() == "operator_cast" {
        let ty = declarator.child_by_field_name("type")?;
        let ty_text = node_text(ty, src)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        return Some((format!("operator {ty_text}"), declarator));
    }
    let func = c::find_function_declarator(declarator)?;
    let name_node = func.child_by_field_name("declarator")?;
    let name = match name_node.kind() {
        "identifier" | "field_identifier" => node_text(name_node, src).to_string(),
        "destructor_name" => node_text(name_node, src).split_whitespace().collect(),
        "operator_name" => normalize_operator(node_text(name_node, src)),
        _ => return None,
    };
    Some((name, name_node))
}

/// `operator ==` → `operator==`; `operator new [ ]` → `operator new[]`.
fn normalize_operator(text: &str) -> String {
    let rest: String = text
        .trim_start_matches("operator")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let rest = rest
        .replace(" [", "[")
        .replace("[ ", "[")
        .replace(" ]", "]");
    let rest = if rest.starts_with(|ch: char| ch.is_alphabetic()) {
        format!(" {rest}")
    } else {
        rest.replace(' ', "")
    };
    format!("operator{rest}")
}

/// A data member. Instance members are Fields; `static` members belong to the
/// class, so they follow the namespace-scope rule: Const when `const` or
/// `constexpr`, Static otherwise.
fn extract_data_member(node: Node, declarator: Node, src: &[u8]) -> Option<ExtractedSymbol> {
    let name_node = c::find_name_node_in_declarator(declarator)?;
    let name = node_text(name_node, src);
    let is_static = c::has_specifier(node, src, "static");
    let kind = if !is_static {
        SymbolKind::Field
    } else if c::has_specifier(node, src, "const") || c::has_specifier(node, src, "constexpr") {
        SymbolKind::Const
    } else {
        SymbolKind::Static
    };
    let signature = node
        .child_by_field_name("type")
        .map(|t| format!("{} {name}", node_text(t, src)));

    let mut sym = base_symbol(node, src, Some(name_node), name, kind);
    sym.signature_hash = signature
        .as_ref()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
    sym.signature = signature;
    sym.docstring = c::extract_docstring(node, src);
    if is_static {
        let mut attrs = serde_json::Map::new();
        attrs.insert("is_static".into(), true.into());
        sym.language_attrs = attrs_json(attrs);
    }
    Some(sym)
}

/// `using X = Y;` → TypeAlias.
fn extract_alias(node: Node, src: &[u8]) -> Option<ExtractedSymbol> {
    let name_node = node.child_by_field_name("name")?;
    let name = node_text(name_node, src);
    let signature = Some(
        node_text(node, src)
            .trim_end_matches(';')
            .trim()
            .to_string(),
    );
    let mut sym = base_symbol(node, src, Some(name_node), name, SymbolKind::TypeAlias);
    sym.signature_hash = signature
        .as_ref()
        .map(|s| blake3::hash(s.as_bytes()).to_hex().to_string());
    sym.signature = signature;
    sym.docstring = c::extract_docstring(node, src);
    sym.visibility = Some("pub".to_string());
    Some(sym)
}

/// A symbol named `name` (unqualified — the enclosing scope prefixes it)
/// spanning `node`, hashed around `name_node`, with every optional field empty
/// for the caller to fill in.
fn base_symbol(
    node: Node,
    src: &[u8],
    name_node: Option<Node>,
    name: &str,
    kind: SymbolKind,
) -> ExtractedSymbol {
    ExtractedSymbol {
        qualified_name: name.to_string(),
        short_name: name.to_string(),
        kind,
        signature: None,
        signature_hash: None,
        structural_hash: Some(structural_hash::compute(
            node,
            src,
            name_node.map(|n| (n.start_byte(), n.end_byte())),
        )),
        visibility: None,
        start_line: node.start_position().row + 1,
        start_col: node.start_position().column,
        end_line: node.end_position().row + 1,
        end_col: node.end_position().column,
        children: vec![],
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        flags: 0,
        language_attrs: None,
    }
}

/// Qualify `sym` and its whole subtree by one enclosing scope.
fn prefix_qualified(sym: &mut ExtractedSymbol, scope: &str) {
    sym.qualified_name = format!("{scope}::{}", sym.qualified_name);
    for child in &mut sym.children {
        prefix_qualified(child, scope);
    }
}

/// Anonymous-namespace members are internal linkage all the way down.
fn make_private(sym: &mut ExtractedSymbol) {
    sym.visibility = Some("private".to_string());
    for child in &mut sym.children {
        make_private(child);
    }
}

/// Apply a member's access section: sutra visibility plus the exact
/// specifier in `language_attrs.access`. Only the member itself — a nested
/// class's own members keep their own access.
fn set_access(sym: &mut ExtractedSymbol, access: Access) {
    sym.visibility = Some(access.visibility().to_string());
    let mut attrs: serde_json::Map<String, serde_json::Value> = match sym.language_attrs.as_deref()
    {
        Some(json) => serde_json::from_str(json)
            .expect("invariant: language_attrs is a JSON map this parser serialized"),
        None => serde_json::Map::new(),
    };
    attrs.insert("access".into(), access.as_str().into());
    sym.language_attrs = attrs_json(attrs);
}

/// Whether `node` has a direct child token of `kind` (`virtual` is an
/// anonymous keyword, not a specifier node).
fn has_token(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|n| n.kind() == kind)
}

fn attrs_json(attrs: serde_json::Map<String, serde_json::Value>) -> Option<String> {
    if attrs.is_empty() {
        return None;
    }
    Some(
        serde_json::to_string(&attrs)
            .expect("invariant: a string-keyed JSON map always serializes"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::adapter::{CppAdapter, ParserPool};
    use crate::parser::c::FLAG_TEST;
    use crate::parser::{RefContextKind, SymbolKind};
    use std::time::Duration;

    fn parse_cpp(code: &str) -> ParseResult {
        let mut pool = ParserPool::new(Duration::from_secs(5));
        pool.parse_with(&CppAdapter, code, "test.cpp").unwrap()
    }

    fn attrs(r: &ParseResult) -> serde_json::Map<String, serde_json::Value> {
        serde_json::from_str(r.symbols[0].language_attrs.as_deref().unwrap()).unwrap()
    }

    fn of_kind(r: &ParseResult, kind: SymbolKind) -> Vec<&crate::parser::ExtractedSymbol> {
        r.symbols.iter().filter(|s| s.kind == kind).collect()
    }

    fn refs_of(r: &ParseResult, kind: RefContextKind) -> Vec<&str> {
        r.references
            .iter()
            .filter(|x| x.context_kind == kind)
            .map(|x| x.name.as_str())
            .collect()
    }

    // --- C-subset parity: one twin per c.rs::tests scenario ---------------

    #[test]
    fn smoke_parse_function() {
        let r = parse_cpp("int main(void) { return 0; }");
        assert!(r.parsed_ok);
        assert_eq!(r.language, "cpp");
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].short_name, "main");
        assert_eq!(r.symbols[0].kind, SymbolKind::Function);
    }

    #[test]
    fn function_signature_extracted() {
        let r = parse_cpp("int add(int a, int b) { return a + b; }");
        let sig = r.symbols[0].signature.as_deref().unwrap();
        assert!(sig.contains("add"));
        assert!(sig.contains("int a"));
        assert!(sig.contains("int b"));
    }

    #[test]
    fn pointer_return_detected() {
        let r = parse_cpp("char *get_name(void) { return 0; }");
        assert_eq!(attrs(&r).get("returns_ptr"), Some(&true.into()));
    }

    #[test]
    fn void_return_detected() {
        let r = parse_cpp("void do_stuff(void) {}");
        assert_eq!(attrs(&r).get("returns_void"), Some(&true.into()));
    }

    #[test]
    fn static_function_is_private() {
        let r = parse_cpp("static int helper(void) { return 1; }");
        assert_eq!(r.symbols[0].visibility.as_deref(), Some("private"));
        assert_eq!(attrs(&r).get("is_static"), Some(&true.into()));
    }

    #[test]
    fn non_static_function_is_pub() {
        let r = parse_cpp("int foo(void) { return 0; }");
        assert_eq!(r.symbols[0].visibility.as_deref(), Some("pub"));
    }

    #[test]
    fn struct_with_body_extracted() {
        let r = parse_cpp("struct Point { int x; int y; };");
        let structs = of_kind(&r, SymbolKind::Struct);
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].short_name, "Point");
    }

    #[test]
    fn enum_with_body_extracted() {
        let r = parse_cpp("enum Color { RED, GREEN, BLUE };");
        let enums = of_kind(&r, SymbolKind::Enum);
        assert_eq!(enums.len(), 1);
        assert_eq!(enums[0].short_name, "Color");
    }

    #[test]
    fn typedef_extracted() {
        let r = parse_cpp("typedef unsigned long size_t;");
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].short_name, "size_t");
        assert_eq!(r.symbols[0].kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn typedef_struct_extracts_both() {
        let r = parse_cpp("typedef struct Node { int val; } Node;");
        let kinds: Vec<_> = r.symbols.iter().map(|s| s.kind).collect();
        assert!(kinds.contains(&SymbolKind::Struct));
        assert!(kinds.contains(&SymbolKind::TypeAlias));
    }

    #[test]
    fn preproc_function_def_is_macro() {
        let r = parse_cpp("#define MAX(a, b) ((a) > (b) ? (a) : (b))");
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].short_name, "MAX");
        assert_eq!(r.symbols[0].kind, SymbolKind::Macro);
    }

    #[test]
    fn preproc_def_is_const() {
        let r = parse_cpp("#define BUFFER_SIZE 1024");
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].short_name, "BUFFER_SIZE");
        assert_eq!(r.symbols[0].kind, SymbolKind::Const);
    }

    #[test]
    fn header_guard_filtered() {
        let r = parse_cpp(
            "#define MY_HEADER_H\n#define MY_HEADER_H_\n#define MY_HEADER_INCLUDED\n#define MY_HEADER_INCLUDED_\n#define REAL_CONST 42",
        );
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].short_name, "REAL_CONST");
    }

    #[test]
    fn global_static_var() {
        let r = parse_cpp("static int count = 0;");
        assert_eq!(r.symbols.len(), 1);
        assert_eq!(r.symbols[0].kind, SymbolKind::Static);
        assert_eq!(r.symbols[0].visibility.as_deref(), Some("private"));
    }

    #[test]
    fn global_const_var() {
        let r = parse_cpp("const int MAX = 100;");
        let consts = of_kind(&r, SymbolKind::Const);
        assert_eq!(consts.len(), 1);
        assert_eq!(consts[0].short_name, "MAX");
    }

    #[test]
    fn docstring_block_comment() {
        let r = parse_cpp("/** Adds two numbers */\nint add(int a, int b) { return a + b; }");
        assert_eq!(r.symbols[0].docstring.as_deref(), Some("Adds two numbers"));
    }

    #[test]
    fn docstring_line_comments() {
        let r = parse_cpp("// First line\n// Second line\nint foo(void) { return 0; }");
        let doc = r.symbols[0].docstring.as_deref().unwrap();
        assert!(doc.contains("First line"));
        assert!(doc.contains("Second line"));
    }

    #[test]
    fn call_expression_ref() {
        let r = parse_cpp("void f(void) { printf(\"hi\"); }");
        assert!(refs_of(&r, RefContextKind::Call).contains(&"printf"));
    }

    #[test]
    fn field_access_ref() {
        let r = parse_cpp("struct S { int x; };\nvoid f(void) { struct S s; s.x = 1; }");
        assert!(refs_of(&r, RefContextKind::FieldAccess).contains(&"x"));
    }

    #[test]
    fn type_use_ref() {
        let r = parse_cpp("typedef int MyInt;\nvoid f(MyInt x) {}");
        assert!(refs_of(&r, RefContextKind::TypeUse).contains(&"MyInt"));
    }

    #[test]
    fn include_quotes_preserved() {
        let r = parse_cpp("#include <stdio.h>\n#include \"myheader.h\"\nvoid f(void) {}");
        assert_eq!(r.imports.len(), 2);
        assert!(r.imports.iter().any(|i| i.raw_path == "<stdio.h>"));
        assert!(r.imports.iter().any(|i| i.raw_path == "\"myheader.h\""));
    }

    #[test]
    fn test_file_heuristic() {
        assert!(is_test_path("foo_test.cc"));
        assert!(is_test_path("db/version_set_unittest.cc"));
        assert!(is_test_path("foo_test.cpp"));
        assert!(is_test_path("foo_test.c"));
        assert!(is_test_path("test_foo.cpp"));
        assert!(is_test_path("tests/bar.cpp"));
        assert!(!is_test_path("main.cpp"));
        assert!(!is_test_path("db/version_set.cc"));
        // The suffix rule is scoped to C/C++ extensions: any_language_is_test_path
        // unions every adapter, so it must not claim other languages' files.
        assert!(!is_test_path("src/foo_test.py"));
    }

    #[test]
    fn test_function_flag() {
        let r = parse_cpp("void test_something(void) {}");
        assert_ne!(r.symbols[0].flags & FLAG_TEST, 0);
    }

    #[test]
    fn test_file_flags_symbols() {
        let mut pool = ParserPool::new(Duration::from_secs(5));
        let r = pool
            .parse_with(
                &CppAdapter,
                "int helper(void) { return 0; }",
                "db/db_test.cc",
            )
            .unwrap();
        assert_ne!(r.symbols[0].flags & FLAG_TEST, 0);
    }

    #[test]
    fn variadic_detected() {
        let r = parse_cpp("int my_printf(const char *fmt, ...) { return 0; }");
        let map = attrs(&r);
        assert_eq!(map.get("is_variadic"), Some(&true.into()));
        assert_eq!(map.get("takes_ptr"), Some(&true.into()));
        assert_eq!(map.get("has_const"), Some(&true.into()));
    }

    #[test]
    fn complexity_computed() {
        let r = parse_cpp("int f(int x) { if (x > 0) { return 1; } else { return 0; } }");
        assert!(r.symbols[0].cyclomatic.unwrap() > 1);
        assert!(r.symbols[0].cognitive.unwrap() > 0);
    }

    #[test]
    fn extern_declaration_skipped() {
        let r = parse_cpp("extern int global_var;");
        assert!(of_kind(&r, SymbolKind::Static).is_empty());
        assert!(of_kind(&r, SymbolKind::Const).is_empty());
    }

    #[test]
    fn inline_detected() {
        let r = parse_cpp("inline int fast(void) { return 1; }");
        assert_eq!(attrs(&r).get("is_inline"), Some(&true.into()));
    }

    #[test]
    fn struct_param_detected() {
        let r = parse_cpp("void f(struct Point p) {}");
        assert_eq!(attrs(&r).get("has_struct_param"), Some(&true.into()));
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
        let r = parse_cpp(code);
        // base 1 + 2*if + for + while + 2*case - switch + && = 7
        assert_eq!(r.symbols[0].cyclomatic, Some(7));
    }

    #[test]
    fn cognitive_scores_nesting() {
        let code = "int f(int x) { if (x > 0) { if (x < 10) { return 1; } } return 0; }";
        let r = parse_cpp(code);
        // outer if: +1 (nesting 0), inner if: +1+1 (nesting 1) = 3
        assert_eq!(r.symbols[0].cognitive, Some(3));
    }

    #[test]
    fn pointer_var_not_in_refs() {
        let r = parse_cpp("int *ptr = 0;");
        assert!(!r.references.iter().any(|r| r.name == "ptr"));
    }

    #[test]
    fn array_var_not_in_refs() {
        let r = parse_cpp("int arr[10];");
        assert!(!r.references.iter().any(|r| r.name == "arr"));
    }

    #[test]
    fn docstring_on_typedef_struct() {
        let r = parse_cpp("/** A node */\ntypedef struct Node { int val; } Node;");
        let s = of_kind(&r, SymbolKind::Struct)[0];
        assert!(s.docstring.as_deref().unwrap().contains("A node"));
    }

    #[test]
    fn docstring_on_declaration_struct() {
        let r = parse_cpp("/** A point */\nstruct Point { int x; int y; } origin;");
        let s = of_kind(&r, SymbolKind::Struct)[0];
        assert!(s.docstring.as_deref().unwrap().contains("A point"));
    }

    #[test]
    fn comma_separated_vars() {
        let r = parse_cpp("static int a, b, c;");
        let names: Vec<_> = of_kind(&r, SymbolKind::Static)
            .iter()
            .map(|s| s.short_name.as_str())
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn comma_separated_typedefs() {
        let r = parse_cpp("typedef int Int, Integer;");
        let names: Vec<_> = of_kind(&r, SymbolKind::TypeAlias)
            .iter()
            .map(|s| s.short_name.as_str())
            .collect();
        assert_eq!(names, ["Int", "Integer"]);
    }

    #[test]
    fn signature_includes_const_qualifier() {
        let r = parse_cpp("const int get_val(void) { return 0; }");
        let sig = r.symbols[0].signature.as_deref().unwrap();
        assert!(sig.starts_with("const"));
    }

    #[test]
    fn ternary_counted_in_cyclomatic() {
        let r = parse_cpp("int f(int x) { return x > 0 ? 1 : 0; }");
        // base 1 + ternary = 2
        assert_eq!(r.symbols[0].cyclomatic, Some(2));
    }

    #[test]
    fn ternary_counted_in_cognitive() {
        let r = parse_cpp("int f(int x) { return x > 0 ? 1 : 0; }");
        // ternary: +1
        assert_eq!(r.symbols[0].cognitive, Some(1));
    }

    #[test]
    fn struct_fields_extracted() {
        let r = parse_cpp("struct Point { int x; int y; };");
        let s = of_kind(&r, SymbolKind::Struct)[0];
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
        let r = parse_cpp("struct Node { struct Node *next; int val; };");
        let s = of_kind(&r, SymbolKind::Struct)[0];
        assert_eq!(s.children.len(), 2);
        let next = s.children.iter().find(|f| f.short_name == "next").unwrap();
        assert_eq!(next.kind, SymbolKind::Field);
        assert_eq!(next.qualified_name, "Node::next");
    }

    #[test]
    fn typedef_struct_fields_extracted() {
        let r = parse_cpp("typedef struct Pair { int a; int b; } Pair;");
        let s = of_kind(&r, SymbolKind::Struct)[0];
        assert_eq!(s.children.len(), 2);
        assert_eq!(s.children[0].short_name, "a");
        assert_eq!(s.children[0].kind, SymbolKind::Field);
    }

    // --- C++-only complexity constructs ------------------------------------

    #[test]
    fn range_for_counted() {
        let r = parse_cpp("int f(int *v) { int s = 0; for (int x : v) { s += x; } return s; }");
        // base 1 + range-for = 2
        assert_eq!(r.symbols[0].cyclomatic, Some(2));
        // range-for: +1 at nesting 0
        assert_eq!(r.symbols[0].cognitive, Some(1));
    }

    #[test]
    fn try_catch_counted() {
        let code = "int f() { try { g(); } catch (const E &e) { return 1; } catch (...) { return 2; } return 0; }";
        let r = parse_cpp(code);
        // base 1 + 2*catch = 3; try itself is not a decision point
        assert_eq!(r.symbols[0].cyclomatic, Some(3));
        // try: +1 (nesting 0); each catch: +1 +1 (nesting 1 under try) = 5
        assert_eq!(r.symbols[0].cognitive, Some(5));
    }

    #[test]
    fn lambda_increments_nesting() {
        let code = "void f(int x) { auto g = [&](int y) { if (y > x) { h(); } }; }";
        let r = parse_cpp(code);
        // if inside the lambda: +1 + nesting 1
        assert_eq!(r.symbols[0].cognitive, Some(2));
        assert_eq!(r.symbols[0].cyclomatic, Some(2));
    }

    #[test]
    fn alternative_logical_tokens_counted() {
        let r = parse_cpp("bool f(bool a, bool b, bool c) { return a and b or c; }");
        // base 1 + and + or = 3
        assert_eq!(r.symbols[0].cyclomatic, Some(3));
        assert_eq!(r.symbols[0].cognitive, Some(2));
    }

    // --- Scopes, classes, members (sutra/528) -----------------------------

    fn flat(r: &ParseResult) -> Vec<&crate::parser::ExtractedSymbol> {
        crate::parser::flatten_symbols(&r.symbols)
    }

    fn sym<'a>(r: &'a ParseResult, qualified: &str) -> &'a crate::parser::ExtractedSymbol {
        flat(r)
            .into_iter()
            .find(|s| s.qualified_name == qualified)
            .unwrap_or_else(|| {
                let names: Vec<_> = flat(r).iter().map(|s| s.qualified_name.clone()).collect();
                panic!("no symbol {qualified}; have {names:?}")
            })
    }

    fn sym_attrs(s: &crate::parser::ExtractedSymbol) -> serde_json::Map<String, serde_json::Value> {
        s.language_attrs
            .as_deref()
            .map(|a| serde_json::from_str(a).unwrap())
            .unwrap_or_default()
    }

    #[test]
    fn nested_namespaces_qualify_members() {
        let r = parse_cpp("namespace a { namespace b { int f() { return 0; } } }");
        assert_eq!(sym(&r, "a").kind, SymbolKind::Module);
        assert_eq!(sym(&r, "a::b").kind, SymbolKind::Module);
        let f = sym(&r, "a::b::f");
        assert_eq!(f.kind, SymbolKind::Function);
        assert_eq!(f.short_name, "f");
    }

    #[test]
    fn nested_namespace_specifier_is_one_module() {
        let r = parse_cpp("namespace a::b { int g() { return 0; } }");
        let ns = sym(&r, "a::b");
        assert_eq!(ns.kind, SymbolKind::Module);
        assert_eq!(ns.short_name, "b");
        sym(&r, "a::b::g");
    }

    #[test]
    fn reopened_namespace_emits_both_modules() {
        let r = parse_cpp(
            "namespace n { int f() { return 0; } }\nnamespace n { int g() { return 1; } }",
        );
        let modules = of_kind(&r, SymbolKind::Module);
        assert_eq!(modules.len(), 2);
        assert!(modules.iter().all(|m| m.qualified_name == "n"));
        sym(&r, "n::f");
        sym(&r, "n::g");
    }

    #[test]
    fn inline_namespace_is_marked_and_qualifies() {
        let r = parse_cpp("namespace lib { inline namespace v1 { int f() { return 0; } } }");
        assert_eq!(
            sym_attrs(sym(&r, "lib::v1")).get("is_inline"),
            Some(&true.into())
        );
        sym(&r, "lib::v1::f");
    }

    #[test]
    fn anonymous_namespace_members_are_private_and_unqualified() {
        let r = parse_cpp("namespace { int helper() { return 0; } struct S { int x; }; }");
        assert!(of_kind(&r, SymbolKind::Module).is_empty());
        assert_eq!(sym(&r, "helper").visibility.as_deref(), Some("private"));
        assert_eq!(sym(&r, "S").visibility.as_deref(), Some("private"));
        assert_eq!(sym(&r, "S::x").visibility.as_deref(), Some("private"));
    }

    #[test]
    fn nested_class_qualified_through_namespace() {
        let r = parse_cpp("namespace ns { class Outer { public: struct Inner { int y; }; }; }");
        sym(&r, "ns::Outer");
        assert_eq!(sym(&r, "ns::Outer::Inner").kind, SymbolKind::Struct);
        assert_eq!(sym(&r, "ns::Outer::Inner::y").kind, SymbolKind::Field);
    }

    #[test]
    fn class_and_struct_record_class_key() {
        let r =
            parse_cpp("class A { int x; };\nstruct B { int y; };\nunion U { int i; float f; };");
        assert_eq!(
            sym_attrs(sym(&r, "A")).get("class_key"),
            Some(&"class".into())
        );
        assert_eq!(
            sym_attrs(sym(&r, "B")).get("class_key"),
            Some(&"struct".into())
        );
        let u = sym(&r, "U");
        assert_eq!(u.kind, SymbolKind::Struct);
        assert_eq!(sym_attrs(u).get("class_key"), Some(&"union".into()));
        assert_eq!(u.children.len(), 2);
    }

    #[test]
    fn class_members_default_private() {
        let r = parse_cpp("class A { int x; void f(); };");
        let x = sym(&r, "A::x");
        assert_eq!(x.visibility.as_deref(), Some("private"));
        assert_eq!(sym_attrs(x).get("access"), Some(&"private".into()));
        assert_eq!(sym(&r, "A::f").visibility.as_deref(), Some("private"));
    }

    #[test]
    fn struct_and_union_members_default_public() {
        let r = parse_cpp("struct S { int x; void f(); };\nunion U { int i; };");
        assert_eq!(sym(&r, "S::x").visibility.as_deref(), Some("pub"));
        assert_eq!(sym(&r, "S::f").visibility.as_deref(), Some("pub"));
        assert_eq!(
            sym_attrs(sym(&r, "U::i")).get("access"),
            Some(&"public".into())
        );
    }

    #[test]
    fn access_specifiers_flip_following_members() {
        let r = parse_cpp(
            "class A {\n int a;\npublic:\n int b;\n void pb();\nprotected:\n int c;\nprivate:\n int d;\n};",
        );
        assert_eq!(sym(&r, "A::a").visibility.as_deref(), Some("private"));
        assert_eq!(sym(&r, "A::b").visibility.as_deref(), Some("pub"));
        assert_eq!(sym(&r, "A::pb").visibility.as_deref(), Some("pub"));
        let c = sym(&r, "A::c");
        assert_eq!(c.visibility.as_deref(), Some("private"));
        assert_eq!(sym_attrs(c).get("access"), Some(&"protected".into()));
        assert_eq!(
            sym_attrs(sym(&r, "A::d")).get("access"),
            Some(&"private".into())
        );
    }

    #[test]
    fn struct_private_section_is_private() {
        let r = parse_cpp("struct S { int a; private: int b; };");
        assert_eq!(sym(&r, "S::a").visibility.as_deref(), Some("pub"));
        assert_eq!(sym(&r, "S::b").visibility.as_deref(), Some("private"));
    }

    #[test]
    fn nested_class_access_does_not_leak_into_its_members() {
        let r = parse_cpp("class A { struct In { int y; }; };");
        assert_eq!(sym(&r, "A::In").visibility.as_deref(), Some("private"));
        assert_eq!(sym(&r, "A::In::y").visibility.as_deref(), Some("pub"));
    }

    #[test]
    fn method_declaration_and_definition() {
        let r = parse_cpp("struct S { void decl(); int def() { if (1) return 1; return 0; } };");
        let decl = sym(&r, "S::decl");
        assert_eq!(decl.kind, SymbolKind::Method);
        assert_eq!(sym_attrs(decl).get("declaration_only"), Some(&true.into()));
        assert_eq!(decl.cyclomatic, None);
        let def = sym(&r, "S::def");
        assert_eq!(def.kind, SymbolKind::Method);
        assert_eq!(sym_attrs(def).get("declaration_only"), None);
        assert_eq!(def.cyclomatic, Some(2));
        assert_eq!(def.signature.as_deref(), Some("int def()"));
    }

    #[test]
    fn ctor_dtor_operator_names_are_stable() {
        let r = parse_cpp(
            "class Foo {\npublic:\n Foo();\n explicit Foo(int);\n ~Foo();\n \
             bool operator==(const Foo&) const;\n Foo& operator = (const Foo&);\n \
             operator bool() const { return true; }\n void* operator new[](unsigned long);\n};",
        );
        let names: Vec<_> = sym(&r, "Foo")
            .children
            .iter()
            .map(|s| s.qualified_name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "Foo::Foo",
                "Foo::Foo",
                "Foo::~Foo",
                "Foo::operator==",
                "Foo::operator=",
                "Foo::operator bool",
                "Foo::operator new[]",
            ]
        );
        assert!(
            sym(&r, "Foo")
                .children
                .iter()
                .all(|s| s.kind == SymbolKind::Method)
        );
        assert_eq!(sym(&r, "Foo::operator bool").cyclomatic, Some(1));
    }

    #[test]
    fn defaulted_deleted_virtual_pure() {
        let r = parse_cpp(
            "class B {\npublic:\n virtual ~B() = default;\n B(const B&) = delete;\n virtual void run() = 0;\n};",
        );
        let dtor = sym_attrs(sym(&r, "B::~B"));
        assert_eq!(dtor.get("is_defaulted"), Some(&true.into()));
        assert_eq!(dtor.get("is_virtual"), Some(&true.into()));
        assert_eq!(dtor.get("declaration_only"), None);
        assert_eq!(
            sym_attrs(sym(&r, "B::B")).get("is_deleted"),
            Some(&true.into())
        );
        let run = sym_attrs(sym(&r, "B::run"));
        assert_eq!(run.get("is_pure_virtual"), Some(&true.into()));
        assert_eq!(run.get("declaration_only"), Some(&true.into()));
    }

    #[test]
    fn reference_returning_functions_and_methods_extracted() {
        let r = parse_cpp(
            "int& ref_fn(int& a) { return a; }\nstruct S { int x; int& get() { return x; } };\nint g; int &r = g;",
        );
        assert_eq!(sym(&r, "ref_fn").kind, SymbolKind::Function);
        assert_eq!(sym(&r, "S::get").kind, SymbolKind::Method);
        assert_eq!(sym(&r, "r").kind, SymbolKind::Static);
    }

    #[test]
    fn static_members_are_const_or_static() {
        let r = parse_cpp(
            "struct S { static int count; static constexpr int K = 3; static const int L = 1; int x; };",
        );
        let count = sym(&r, "S::count");
        assert_eq!(count.kind, SymbolKind::Static);
        assert_eq!(sym_attrs(count).get("is_static"), Some(&true.into()));
        assert_eq!(sym(&r, "S::K").kind, SymbolKind::Const);
        assert_eq!(sym(&r, "S::L").kind, SymbolKind::Const);
        assert_eq!(sym(&r, "S::x").kind, SymbolKind::Field);
    }

    #[test]
    fn multiple_member_declarators_each_emitted() {
        let r = parse_cpp("struct S { int a, b; };");
        sym(&r, "S::a");
        sym(&r, "S::b");
    }

    #[test]
    fn scoped_enum_with_qualified_enumerators() {
        let r = parse_cpp("namespace n { enum class Color : int { Red, Green }; }");
        let e = sym(&r, "n::Color");
        assert_eq!(e.kind, SymbolKind::Enum);
        assert_eq!(sym_attrs(e).get("is_scoped"), Some(&true.into()));
        assert_eq!(sym(&r, "n::Color::Red").kind, SymbolKind::Const);
        sym(&r, "n::Color::Green");
    }

    #[test]
    fn unscoped_enum_enumerators_and_nested_enum() {
        let r = parse_cpp("enum Dir { Up, Down };\nclass A { enum struct Mode { On }; };");
        assert_eq!(sym_attrs(sym(&r, "Dir")).get("is_scoped"), None);
        sym(&r, "Dir::Up");
        let mode = sym(&r, "A::Mode");
        assert_eq!(mode.visibility.as_deref(), Some("private"));
        assert_eq!(sym_attrs(mode).get("is_scoped"), Some(&true.into()));
        sym(&r, "A::Mode::On");
    }

    #[test]
    fn using_alias_and_typedef_in_scopes() {
        let r = parse_cpp(
            "namespace n { using Id = int; }\nclass A { using V = int; typedef long T; };",
        );
        assert_eq!(sym(&r, "n::Id").kind, SymbolKind::TypeAlias);
        let v = sym(&r, "A::V");
        assert_eq!(v.kind, SymbolKind::TypeAlias);
        assert_eq!(v.visibility.as_deref(), Some("private"));
        assert_eq!(sym(&r, "A::T").kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn namespace_scope_static_is_private() {
        let r =
            parse_cpp("namespace n { static int helper() { return 0; } int api() { return 1; } }");
        assert_eq!(sym(&r, "n::helper").visibility.as_deref(), Some("private"));
        assert_eq!(sym(&r, "n::api").visibility.as_deref(), Some("pub"));
    }

    #[test]
    fn base_classes_emit_type_use_refs() {
        let r = parse_cpp("class Foo : public Base, private ns::Other { };");
        let types = refs_of(&r, RefContextKind::TypeUse);
        assert!(types.contains(&"Base"), "{types:?}");
        assert!(types.contains(&"Other"), "{types:?}");
        assert!(
            !types.contains(&"Foo"),
            "class name is a definition: {types:?}"
        );
    }

    #[test]
    fn header_guard_and_extern_c_blocks_are_transparent() {
        let r = parse_cpp(
            "#ifndef G_H\n#define G_H\nclass K { public: void m(); };\n\
             extern \"C\" { int cfn() { return 1; } }\n#endif",
        );
        sym(&r, "K::m");
        assert_eq!(sym(&r, "cfn").kind, SymbolKind::Function);
        assert!(flat(&r).iter().all(|s| s.short_name != "G_H"));
    }

    #[test]
    fn access_carries_through_preproc_in_class() {
        let r = parse_cpp("class A {\npublic:\n#ifdef X\n int a;\n#endif\n int b;\n};");
        assert_eq!(sym(&r, "A::a").visibility.as_deref(), Some("pub"));
        assert_eq!(sym(&r, "A::b").visibility.as_deref(), Some("pub"));
    }
}
