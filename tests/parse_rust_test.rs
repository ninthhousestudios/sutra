use sutra::parser::RefContextKind;
use sutra::parser::SymbolKind;
use sutra::parser::{flatten_symbols, parse_file};

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

    // Top-level: Struct(Foo) and Impl(Foo). Method(bar) nested under Impl.
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
    assert_eq!(impl_sym.children.len(), 1);

    let method_sym = &impl_sym.children[0];
    assert_eq!(method_sym.short_name, "bar");
    assert_eq!(method_sym.qualified_name, "Foo::bar");
    assert_eq!(method_sym.kind, SymbolKind::Method);
    assert!(method_sym.signature.is_some());
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

    let flat = flatten_symbols(&result.symbols);

    let trait_sym = flat
        .iter()
        .find(|s| s.kind == SymbolKind::Trait)
        .expect("should have a trait");
    assert_eq!(trait_sym.short_name, "Drawable");

    let trait_methods: Vec<_> = flat
        .iter()
        .filter(|s| s.kind == SymbolKind::Method && s.qualified_name.starts_with("Drawable::"))
        .collect();
    assert!(
        !trait_methods.is_empty(),
        "trait should have method(s): {:#?}",
        result.symbols
    );

    let impl_sym = flat
        .iter()
        .find(|s| s.kind == SymbolKind::Impl)
        .expect("should have an impl");
    assert_eq!(impl_sym.short_name, "Circle");

    let impl_methods: Vec<_> = flat
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
    let flat = flatten_symbols(&result.symbols);
    if let Some(sym) = flat.iter().find(|s| s.short_name == "broken") {
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

#[test]
fn test_tree_structured_symbols() {
    let src = r#"
mod inner {
    struct Foo;
    impl Foo {
        fn bar(&self) {}
    }
}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    assert_eq!(result.symbols.len(), 1);
    let module = &result.symbols[0];
    assert_eq!(module.short_name, "inner");
    assert_eq!(module.kind, SymbolKind::Module);
    assert_eq!(module.children.len(), 2);

    let struct_sym = module
        .children
        .iter()
        .find(|s| s.kind == SymbolKind::Struct)
        .expect("module should contain struct");
    assert_eq!(struct_sym.short_name, "Foo");
    assert!(struct_sym.children.is_empty());

    let impl_sym = module
        .children
        .iter()
        .find(|s| s.kind == SymbolKind::Impl)
        .expect("module should contain impl");
    assert_eq!(impl_sym.children.len(), 1);
    assert_eq!(impl_sym.children[0].short_name, "bar");
    assert_eq!(impl_sym.children[0].kind, SymbolKind::Method);

    let flat = flatten_symbols(&result.symbols);
    assert_eq!(flat.len(), 4);
}

#[test]
fn test_flatten_preserves_dfs_order() {
    let src = r#"
fn top() {}
impl Foo {
    fn method_a(&self) {}
    fn method_b(&self) {}
}
struct Foo;
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    let flat = flatten_symbols(&result.symbols);
    let names: Vec<&str> = flat.iter().map(|s| s.short_name.as_str()).collect();
    assert_eq!(names, vec!["top", "Foo", "method_a", "method_b", "Foo"]);
}

#[test]
fn test_language_attrs_async_unsafe() {
    let src = r#"
async fn fetch() -> Result<(), Error> {}
unsafe fn danger() {}
fn plain() {}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();

    let fetch = result.symbols.iter().find(|s| s.short_name == "fetch").unwrap();
    let attrs: serde_json::Value = serde_json::from_str(
        fetch.language_attrs.as_deref().expect("fetch should have language_attrs"),
    )
    .unwrap();
    assert_eq!(attrs["is_async"], true);
    assert_eq!(attrs["returns_result"], true);

    let danger = result.symbols.iter().find(|s| s.short_name == "danger").unwrap();
    let attrs: serde_json::Value = serde_json::from_str(
        danger.language_attrs.as_deref().expect("danger should have language_attrs"),
    )
    .unwrap();
    assert_eq!(attrs["is_unsafe"], true);

    let plain = result.symbols.iter().find(|s| s.short_name == "plain").unwrap();
    assert!(plain.language_attrs.is_none(), "plain fn should have no language_attrs");
}

#[test]
fn test_language_attrs_self_params() {
    let src = r#"
struct Foo;
impl Foo {
    fn by_ref(&self) {}
    fn by_mut(&mut self) {}
    fn no_self(x: i32) {}
}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let by_ref = flat.iter().find(|s| s.short_name == "by_ref").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(by_ref.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["takes_self_ref"], true);
    assert!(attrs.get("takes_self_mut").is_none());

    let by_mut = flat.iter().find(|s| s.short_name == "by_mut").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(by_mut.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["takes_self_mut"], true);

    let no_self = flat.iter().find(|s| s.short_name == "no_self").unwrap();
    assert!(no_self.language_attrs.is_none());
}

#[test]
fn test_language_attrs_lifetime_params() {
    let src = r#"
struct Borrowed<'a> {
    data: &'a str,
}
fn no_lt() {}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();

    let borrowed = result.symbols.iter().find(|s| s.short_name == "Borrowed").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(borrowed.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["has_lifetime_params"], true);

    let no_lt = result.symbols.iter().find(|s| s.short_name == "no_lt").unwrap();
    assert!(no_lt.language_attrs.is_none());
}

#[test]
fn test_language_attrs_no_false_positive_on_wrapper_types() {
    let src = r#"
fn get_query_result() -> QueryResult {}
fn get_optional_config() -> OptionalConfig {}
fn get_real_result() -> Result<(), Error> {}
fn get_scoped_result() -> anyhow::Result<()> {}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();

    let query_result = result.symbols.iter().find(|s| s.short_name == "get_query_result").unwrap();
    assert!(
        query_result.language_attrs.is_none(),
        "QueryResult should not trigger returns_result: {:?}",
        query_result.language_attrs,
    );

    let optional_config = result.symbols.iter().find(|s| s.short_name == "get_optional_config").unwrap();
    assert!(
        optional_config.language_attrs.is_none(),
        "OptionalConfig should not trigger returns_option: {:?}",
        optional_config.language_attrs,
    );

    let real_result = result.symbols.iter().find(|s| s.short_name == "get_real_result").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(real_result.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["returns_result"], true);

    let scoped_result = result.symbols.iter().find(|s| s.short_name == "get_scoped_result").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(scoped_result.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["returns_result"], true);
}

#[test]
fn test_language_attrs_returns_self() {
    let src = r#"
struct Foo;
impl Foo {
    fn new() -> Self { Foo }
    fn borrow(&self) -> &Self { self }
    fn other() -> Foo { Foo }
}
"#;
    let result = parse_file(src, "rust", "test.rs").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let new_fn = flat.iter().find(|s| s.short_name == "new").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(new_fn.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["returns_self"], true);

    let borrow_fn = flat.iter().find(|s| s.short_name == "borrow").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(borrow_fn.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["returns_self"], true);

    let other_fn = flat.iter().find(|s| s.short_name == "other").unwrap();
    assert!(
        other_fn.language_attrs.is_none()
            || !other_fn.language_attrs.as_deref().unwrap_or("").contains("returns_self"),
        "-> Foo should not trigger returns_self: {:?}",
        other_fn.language_attrs,
    );
}
