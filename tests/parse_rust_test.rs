use sutra::parser::RefContextKind;
use sutra::parser::SymbolKind;
use sutra::parser::parse_file;

#[test]
fn test_parse_function() {
    let src = "fn foo(x: i32) -> bool { true }";
    let result = parse_file(src, "rust", "test.rs").unwrap();

    assert!(result.parsed_ok);
    assert_eq!(result.symbols.len(), 1);

    let sym = &result.symbols[0];
    assert_eq!(sym.short_name, "foo");
    assert_eq!(sym.qualified_name, "foo");
    assert_eq!(sym.kind, SymbolKind::Function);
    assert!(sym.signature.is_some());

    let sig = sym.signature.as_ref().unwrap();
    assert!(sig.contains("fn foo"), "signature was: {sig}");
    assert!(sig.contains("x: i32"), "signature was: {sig}");
    assert!(sig.contains("bool"), "signature was: {sig}");

    // Signature hash should be present when signature exists
    assert!(sym.signature_hash.is_some());

    // Span: 1-indexed lines
    assert_eq!(sym.start_line, 1);
    assert_eq!(sym.start_col, 0);
    assert_eq!(sym.end_line, 1);
}

#[test]
fn test_parse_struct_with_methods() {
    let src = r#"
struct Foo {
    x: i32,
}

impl Foo {
    fn bar(&self) -> i32 {
        self.x
    }
}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    assert!(result.parsed_ok);

    // Should have: Struct(Foo), Impl(Foo), Method(bar)
    let struct_sym = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Struct)
        .expect("should have a struct");
    assert_eq!(struct_sym.short_name, "Foo");

    let impl_sym = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Impl)
        .expect("should have an impl");
    assert_eq!(impl_sym.short_name, "Foo");

    let method_sym = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Method)
        .expect("should have a method");
    assert_eq!(method_sym.short_name, "bar");
    assert_eq!(method_sym.qualified_name, "Foo::bar");
    assert!(method_sym.signature.is_some());
    assert!(
        method_sym.parent_qualified_name.as_deref() == Some("Foo"),
        "parent was: {:?}",
        method_sym.parent_qualified_name
    );
}

#[test]
fn test_parse_trait_impl() {
    let src = r#"
trait Drawable {
    fn draw(&self);
}

impl Drawable for Circle {
    fn draw(&self) {}
}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    assert!(result.parsed_ok);

    let trait_sym = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Trait)
        .expect("should have a trait");
    assert_eq!(trait_sym.short_name, "Drawable");

    // The trait's draw method
    let trait_methods: Vec<_> = result
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Method && s.qualified_name.starts_with("Drawable::"))
        .collect();
    assert!(
        !trait_methods.is_empty(),
        "trait should have method(s): {:#?}",
        result.symbols
    );

    // The impl block — name should be the type being implemented (Circle)
    let impl_sym = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Impl)
        .expect("should have an impl");
    assert_eq!(impl_sym.short_name, "Circle");

    // The impl's draw method should be qualified under Circle
    let impl_methods: Vec<_> = result
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Method && s.qualified_name.starts_with("Circle::"))
        .collect();
    assert!(
        !impl_methods.is_empty(),
        "impl should have method(s): {:#?}",
        result.symbols
    );
}

#[test]
fn test_parse_enum_variants() {
    let src = "enum Color { Red, Blue, Green }";
    let result = parse_file(src, "rust", "test.rs").unwrap();
    assert!(result.parsed_ok);

    let enum_sym = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Enum)
        .expect("should have an enum");
    assert_eq!(enum_sym.short_name, "Color");
    assert_eq!(enum_sym.qualified_name, "Color");
}

#[test]
fn test_parse_use_statements() {
    let src = "use std::collections::HashMap;";
    let result = parse_file(src, "rust", "test.rs").unwrap();
    assert!(result.parsed_ok);

    assert!(
        !result.imports.is_empty(),
        "should have at least one import"
    );
    let import = &result.imports[0];
    assert!(
        import.raw_path.contains("std::collections::HashMap"),
        "raw_path was: {}",
        import.raw_path
    );
    assert_eq!(import.line, 1);
}

#[test]
fn test_parse_visibility() {
    let src = r#"
pub fn a() {}
pub(crate) fn b() {}
fn c() {}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    assert!(result.parsed_ok);

    let a = result
        .symbols
        .iter()
        .find(|s| s.short_name == "a")
        .expect("should have fn a");
    assert_eq!(a.visibility.as_deref(), Some("pub"));

    let b = result
        .symbols
        .iter()
        .find(|s| s.short_name == "b")
        .expect("should have fn b");
    assert_eq!(b.visibility.as_deref(), Some("pub(crate)"));

    let c = result
        .symbols
        .iter()
        .find(|s| s.short_name == "c")
        .expect("should have fn c");
    assert!(c.visibility.is_none());
}

#[test]
fn test_parse_docstrings() {
    let src = "/// This is a doc\nfn documented() {}";
    let result = parse_file(src, "rust", "test.rs").unwrap();
    assert!(result.parsed_ok);

    let sym = result
        .symbols
        .iter()
        .find(|s| s.short_name == "documented")
        .expect("should have fn documented");
    assert!(sym.docstring.is_some(), "docstring should be present");
    let doc = sym.docstring.as_ref().unwrap();
    assert!(doc.contains("This is a doc"), "docstring was: {doc}");
}

#[test]
fn test_parse_error_tolerance() {
    // Broken syntax — tree-sitter should still partially parse
    let src = "fn broken( { }";
    let result = parse_file(src, "rust", "test.rs").unwrap();

    assert!(!result.parsed_ok, "should detect parse errors");
    // tree-sitter may or may not extract the function name depending on the error,
    // but it should not panic and should return a result
    // Check that we got a result with the right file path
    assert_eq!(result.file_path, "test.rs");
    assert_eq!(result.language, "rust");

    // Even with errors, tree-sitter often extracts partial information.
    // The function name "broken" may or may not be found depending on how
    // tree-sitter recovers, so we just verify we don't crash.
    if let Some(sym) = result.symbols.iter().find(|s| s.short_name == "broken") {
        // If it was found, it should be a function
        assert!(
            sym.kind == SymbolKind::Function || sym.kind == SymbolKind::Method,
            "kind was: {:?}",
            sym.kind
        );
    }
}

#[test]
fn test_parse_braced_imports() {
    let src = r#"
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    assert!(result.parsed_ok);

    let paths: Vec<&str> = result.imports.iter().map(|i| i.raw_path.as_str()).collect();
    assert!(
        paths.contains(&"std::collections::HashMap"),
        "paths: {paths:?}"
    );
    assert!(
        paths.contains(&"std::collections::HashSet"),
        "paths: {paths:?}"
    );
    assert!(
        paths.contains(&"std::io"),
        "self should expand to prefix: {paths:?}"
    );
    assert!(paths.contains(&"std::io::Read"), "paths: {paths:?}");
    assert!(paths.contains(&"std::io::Write"), "paths: {paths:?}");
}

#[test]
fn test_struct_literal_has_construction_context() {
    let src = r#"
struct Config {
    name: String,
    count: usize,
}

fn make() -> Config {
    Config { name: "foo".into(), count: 42 }
}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    let config_refs: Vec<_> = result
        .references
        .iter()
        .filter(|r| r.name == "Config")
        .collect();

    let construction_refs: Vec<_> = config_refs
        .iter()
        .filter(|r| r.context_kind == RefContextKind::Construction)
        .collect();
    assert_eq!(
        construction_refs.len(),
        1,
        "expected exactly one Construction ref to Config, got: {config_refs:?}"
    );
}

#[test]
fn test_struct_literal_with_spread_has_construction_context() {
    let src = r#"
struct Config {
    name: String,
    count: usize,
}

fn make(default: Config) -> Config {
    Config { name: "foo".into(), ..default }
}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    let construction_refs: Vec<_> = result
        .references
        .iter()
        .filter(|r| r.name == "Config" && r.context_kind == RefContextKind::Construction)
        .collect();
    assert_eq!(
        construction_refs.len(),
        1,
        "spread struct literal should still be Construction: {construction_refs:?}"
    );
}

#[test]
fn test_tuple_struct_construction_stays_call() {
    let src = r#"
struct Wrapper(i32);

fn make() -> Wrapper {
    Wrapper(42)
}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    let wrapper_refs: Vec<_> = result
        .references
        .iter()
        .filter(|r| r.name == "Wrapper")
        .collect();

    let call_refs: Vec<_> = wrapper_refs
        .iter()
        .filter(|r| r.context_kind == RefContextKind::Call)
        .collect();
    assert!(
        !call_refs.is_empty(),
        "tuple struct construction should be Call, got: {wrapper_refs:?}"
    );

    let construction_refs: Vec<_> = wrapper_refs
        .iter()
        .filter(|r| r.context_kind == RefContextKind::Construction)
        .collect();
    assert!(
        construction_refs.is_empty(),
        "tuple struct should NOT be Construction: {wrapper_refs:?}"
    );
}

#[test]
fn test_scoped_struct_literal_has_construction_context() {
    let src = r#"
mod inner {
    pub struct Foo {
        pub x: i32,
    }
}

fn make() -> inner::Foo {
    inner::Foo { x: 1 }
}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    let foo_refs: Vec<_> = result
        .references
        .iter()
        .filter(|r| r.name == "Foo")
        .collect();

    let construction_refs: Vec<_> = foo_refs
        .iter()
        .filter(|r| r.context_kind == RefContextKind::Construction)
        .collect();
    assert!(
        !construction_refs.is_empty(),
        "scoped struct literal (inner::Foo {{ .. }}) should have Construction ref: {foo_refs:?}"
    );

    // The qualifier 'inner' must NOT be classified as Construction
    let inner_refs: Vec<_> = result
        .references
        .iter()
        .filter(|r| r.name == "inner" && r.context_kind == RefContextKind::Construction)
        .collect();
    assert!(
        inner_refs.is_empty(),
        "qualifier 'inner' should not be Construction: {inner_refs:?}"
    );
}

#[test]
fn test_turbofish_struct_literal_has_construction_context() {
    let src = r#"
struct Foo<T> {
    x: T,
}

fn make() -> Foo<i32> {
    Foo::<i32> { x: 42 }
}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    let foo_refs: Vec<_> = result
        .references
        .iter()
        .filter(|r| r.name == "Foo")
        .collect();

    let construction_refs: Vec<_> = foo_refs
        .iter()
        .filter(|r| r.context_kind == RefContextKind::Construction)
        .collect();
    assert!(
        !construction_refs.is_empty(),
        "turbofish struct literal (Foo::<i32> {{ .. }}) should have Construction ref: {foo_refs:?}"
    );
}
