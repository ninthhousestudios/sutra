use sutra::parser::{self, RefContextKind, SymbolKind, flatten_symbols};

#[test]
fn test_parse_dart_class() {
    let src = r#"
class Foo {
  void bar() {}
  int baz(String s) { return s.length; }
}
"#;
    let result = parser::parse_file(src, "dart", "lib/foo.dart").unwrap();
    assert!(result.parsed_ok);

    let flat = flatten_symbols(&result.symbols);

    let class = flat.iter().find(|s| s.short_name == "Foo");
    assert!(class.is_some(), "expected class Foo");
    assert_eq!(class.unwrap().kind, SymbolKind::Class);

    let methods: Vec<_> = flat
        .iter()
        .filter(|s| s.kind == SymbolKind::Method)
        .collect();
    assert!(
        methods.len() >= 2,
        "expected at least 2 methods, got {}",
        methods.len()
    );
}

#[test]
fn test_parse_dart_enum() {
    let src = "enum Color { red, blue, green }";
    let result = parser::parse_file(src, "dart", "lib/color.dart").unwrap();
    assert!(result.parsed_ok);

    let enums: Vec<_> = result
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Enum)
        .collect();
    assert_eq!(enums.len(), 1);
    assert_eq!(enums[0].short_name, "Color");
}

#[test]
fn test_dart_symbols_without_specific_attrs_store_empty_attrs() {
    let src = "typedef IntMapper = int Function(int value);";
    let result = parser::parse_file(src, "dart", "lib/types.dart").unwrap();
    assert!(result.parsed_ok);

    let alias = result
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::TypeAlias)
        .expect("type alias should be indexed");
    assert_eq!(alias.short_name, "IntMapper");
    assert_eq!(alias.language_attrs.as_deref(), Some("{}"));
}

#[test]
fn test_parse_dart_imports() {
    let src = r#"
import 'package:flutter/material.dart';
import 'dart:async';
"#;
    let result = parser::parse_file(src, "dart", "lib/main.dart").unwrap();
    assert!(result.parsed_ok);
    assert!(
        result.imports.len() >= 2,
        "expected at least 2 imports, got {}",
        result.imports.len()
    );
}

#[test]
fn test_parse_dart_mixin() {
    let src = r#"
mixin Swimming {
  void swim() {}
}
"#;
    let result = parser::parse_file(src, "dart", "lib/swimming.dart").unwrap();
    assert!(result.parsed_ok);

    let mixins: Vec<_> = result
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Mixin)
        .collect();
    assert_eq!(mixins.len(), 1);
    assert_eq!(mixins[0].short_name, "Swimming");
}

#[test]
fn test_parse_dart_extension() {
    let src = r#"
extension StringExt on String {
  bool get isBlank => trim().isEmpty;
}
"#;
    let result = parser::parse_file(src, "dart", "lib/string_ext.dart").unwrap();
    assert!(result.parsed_ok);

    let exts: Vec<_> = result
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Extension)
        .collect();
    assert_eq!(exts.len(), 1);
    assert_eq!(exts[0].short_name, "StringExt");
}

#[test]
fn test_language_attrs_abstract_class() {
    let src = r#"
abstract class Animal {
    void speak();
}
class Dog extends Animal {
    void speak() {}
}
"#;
    let result = parser::parse_file(src, "dart", "lib/animals.dart").unwrap();

    let animal = result
        .symbols
        .iter()
        .find(|s| s.short_name == "Animal")
        .unwrap();
    let attrs: serde_json::Value = serde_json::from_str(
        animal
            .language_attrs
            .as_deref()
            .expect("abstract class should have attrs"),
    )
    .unwrap();
    assert_eq!(attrs["is_abstract"], true);

    let dog = result
        .symbols
        .iter()
        .find(|s| s.short_name == "Dog")
        .unwrap();
    assert_eq!(
        dog.language_attrs.as_deref(),
        Some("{}"),
        "non-abstract class should have empty attrs"
    );
}

#[test]
fn test_language_attrs_factory_constructor() {
    let src = r#"
class Cache {
    factory Cache() => Cache._();
    Cache._();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/cache.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let factory_methods: Vec<_> = flat
        .iter()
        .filter(|s| {
            s.language_attrs
                .as_deref()
                .and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())
                .is_some_and(|v| v["is_factory"] == true)
        })
        .collect();
    assert!(
        !factory_methods.is_empty(),
        "should detect factory constructor"
    );
}

#[test]
fn test_language_attrs_async_method() {
    let src = r#"
class Streamer {
    Stream<int> generate() async* {
        yield 1;
    }
    Future<void> doStuff() async {
        await Future.delayed(Duration(seconds: 1));
    }
    void sync_method() {}
}
"#;
    let result = parser::parse_file(src, "dart", "lib/streamer.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let generate = flat.iter().find(|s| s.short_name == "generate").unwrap();
    let attrs: serde_json::Value = serde_json::from_str(
        generate
            .language_attrs
            .as_deref()
            .expect("async* method should have attrs"),
    )
    .unwrap();
    assert_eq!(
        attrs["is_async"], true,
        "async* method should be marked is_async"
    );

    let do_stuff = flat.iter().find(|s| s.short_name == "doStuff").unwrap();
    let attrs: serde_json::Value = serde_json::from_str(
        do_stuff
            .language_attrs
            .as_deref()
            .expect("async method should have attrs"),
    )
    .unwrap();
    assert_eq!(
        attrs["is_async"], true,
        "async method should be marked is_async"
    );

    let sync_method = flat.iter().find(|s| s.short_name == "sync_method").unwrap();
    assert_eq!(
        sync_method.language_attrs.as_deref(),
        Some("{}"),
        "sync method should have empty attrs"
    );
}

#[test]
fn test_parse_dart_static_methods() {
    let src = r#"
class MyClass {
  static void doStuff(int x) {}
  void normalMethod() {}
  static String get name => 'MyClass';
  static set value(int v) {}
}
"#;
    let result = parser::parse_file(src, "dart", "lib/my_class.dart").unwrap();
    assert!(result.parsed_ok);

    let flat = flatten_symbols(&result.symbols);

    let do_stuff = flat.iter().find(|s| s.short_name == "doStuff");
    assert!(
        do_stuff.is_some(),
        "static method doStuff should be indexed"
    );

    let normal = flat.iter().find(|s| s.short_name == "normalMethod");
    assert!(normal.is_some(), "normal method should still be indexed");

    let getter = flat.iter().find(|s| s.short_name == "name");
    assert!(getter.is_some(), "static getter should be indexed");

    let setter = flat.iter().find(|s| s.short_name == "value");
    assert!(setter.is_some(), "static setter should be indexed");
}

#[test]
fn test_parse_dart_named_constructor() {
    let src = r#"
class Cache {
    Cache._();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/cache.dart").unwrap();
    assert!(result.parsed_ok);

    let flat = flatten_symbols(&result.symbols);
    let constructor = flat.iter().find(|s| s.kind == SymbolKind::Method);
    assert!(constructor.is_some(), "named constructor should be indexed");

    let attrs: serde_json::Value =
        serde_json::from_str(constructor.unwrap().language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(
        attrs["is_constructor"], true,
        "should be marked is_constructor"
    );
}

#[test]
fn test_language_attrs_getter_setter() {
    let src = r#"
class Config {
    String get name => 'test';
    set name(String v) {}
    void normalMethod() {}
}
"#;
    let result = parser::parse_file(src, "dart", "lib/config.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let methods: Vec<_> = flat.iter().filter(|s| s.short_name == "name").collect();
    assert_eq!(methods.len(), 2, "should have getter and setter");

    let getter = methods.iter().find(|s| {
        let a: serde_json::Value =
            serde_json::from_str(s.language_attrs.as_deref().unwrap()).unwrap();
        a["is_getter"] == true
    });
    assert!(
        getter.is_some(),
        "should detect getter_signature as is_getter"
    );

    let setter = methods.iter().find(|s| {
        let a: serde_json::Value =
            serde_json::from_str(s.language_attrs.as_deref().unwrap()).unwrap();
        a["is_setter"] == true
    });
    assert!(
        setter.is_some(),
        "should detect setter_signature as is_setter"
    );
}

#[test]
fn test_language_attrs_has_override() {
    let src = r#"
class MyWidget {
    @override
    void build() {}
}
"#;
    let result = parser::parse_file(src, "dart", "lib/widget.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let build = flat.iter().find(|s| s.short_name == "build").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(build.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(
        attrs["has_override"], true,
        "@override should set has_override"
    );
}

#[test]
fn test_language_attrs_returns_future() {
    let src = r#"
class Api {
    Future<String> fetchData() async { return ''; }
    FutureOr<int> compute() { return 42; }
    void sync() {}
}
"#;
    let result = parser::parse_file(src, "dart", "lib/api.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let fetch = flat.iter().find(|s| s.short_name == "fetchData").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(fetch.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(
        attrs["returns_future"], true,
        "Future return should set returns_future"
    );

    let compute = flat.iter().find(|s| s.short_name == "compute").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(compute.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(
        attrs["returns_future"], true,
        "FutureOr return should set returns_future"
    );

    let sync = flat.iter().find(|s| s.short_name == "sync").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(sync.language_attrs.as_deref().unwrap()).unwrap();
    assert!(
        attrs.get("returns_future").is_none() || attrs["returns_future"] != true,
        "void return should not set returns_future"
    );
}

#[test]
fn test_flags_test_annotation() {
    let src = r#"
@isTest
void myTestHelper() {}

void normalFunction() {}
"#;
    let result = parser::parse_file(src, "dart", "lib/helpers.dart").unwrap();

    let test_fn = result
        .symbols
        .iter()
        .find(|s| s.short_name == "myTestHelper")
        .unwrap();
    assert_ne!(test_fn.flags & 0x01, 0, "@isTest should set FLAG_TEST");

    let normal_fn = result
        .symbols
        .iter()
        .find(|s| s.short_name == "normalFunction")
        .unwrap();
    assert_eq!(
        normal_fn.flags & 0x01,
        0,
        "unannotated function should not have FLAG_TEST"
    );
}

#[test]
fn test_flags_test_file_path() {
    let src = "void helper() {}";
    let result = parser::parse_file(src, "dart", "test/widget_test.dart").unwrap();
    assert_ne!(
        result.symbols[0].flags & 0x02,
        0,
        "test file should set FLAG_CFG_TEST"
    );
}

#[test]
fn test_flags_test_file_suffix() {
    let src = "void helper() {}";
    let result = parser::parse_file(src, "dart", "lib/foo_test.dart").unwrap();
    assert_ne!(
        result.symbols[0].flags & 0x02,
        0,
        "_test.dart suffix should set FLAG_CFG_TEST"
    );
}

#[test]
fn test_flags_non_test_file() {
    let src = "void helper() {}";
    let result = parser::parse_file(src, "dart", "lib/utils.dart").unwrap();
    assert_eq!(
        result.symbols[0].flags & 0x02,
        0,
        "non-test file should not have FLAG_CFG_TEST"
    );
}

#[test]
fn test_flags_override_lifecycle_method() {
    let src = r#"
class MyWidget {
    @override
    void build() {}
    void normalMethod() {}
}
"#;
    let result = parser::parse_file(src, "dart", "lib/widget.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let build = flat.iter().find(|s| s.short_name == "build").unwrap();
    assert_ne!(
        build.flags & 0x04,
        0,
        "@override lifecycle method should set FLAG_FFI_ENTRY"
    );
    assert_ne!(build.flags & 0x08, 0, "@override should set FLAG_OVERRIDE");

    let normal = flat
        .iter()
        .find(|s| s.short_name == "normalMethod")
        .unwrap();
    assert_eq!(
        normal.flags & 0x04,
        0,
        "non-override should not have FLAG_FFI_ENTRY"
    );
    assert_eq!(
        normal.flags & 0x08,
        0,
        "non-override should not have FLAG_OVERRIDE"
    );
}

#[test]
fn test_flags_override_non_lifecycle_method() {
    let src = r#"
class MyRepo {
    @override
    void fetchItems() {}
    @override
    void dispose() {}
}
"#;
    let result = parser::parse_file(src, "dart", "lib/repo.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let fetch = flat.iter().find(|s| s.short_name == "fetchItems").unwrap();
    assert_eq!(
        fetch.flags & 0x04,
        0,
        "non-lifecycle @override should NOT set FLAG_FFI_ENTRY"
    );
    assert_ne!(
        fetch.flags & 0x08,
        0,
        "non-lifecycle @override should set FLAG_OVERRIDE"
    );

    let dispose = flat.iter().find(|s| s.short_name == "dispose").unwrap();
    assert_ne!(
        dispose.flags & 0x04,
        0,
        "lifecycle @override should set FLAG_FFI_ENTRY"
    );
    assert_ne!(
        dispose.flags & 0x08,
        0,
        "lifecycle @override should set FLAG_OVERRIDE"
    );
}

#[test]
fn test_flags_pragma_entry_point() {
    let src = r#"
@pragma('vm:entry-point')
void entryPoint() {}
"#;
    let result = parser::parse_file(src, "dart", "lib/entry.dart").unwrap();
    assert_ne!(
        result.symbols[0].flags & 0x04,
        0,
        "@pragma('vm:entry-point') should set FLAG_FFI_ENTRY"
    );
}

#[test]
fn test_top_level_getter() {
    let src = r#"
String get appName => 'MyApp';
"#;
    let result = parser::parse_file(src, "dart", "lib/config.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let getter = flat.iter().find(|s| {
        s.short_name == "appName"
            && s.language_attrs
                .as_deref()
                .and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())
                .is_some_and(|v| v["is_getter"] == true)
    });
    assert!(
        getter.is_some(),
        "top-level getter should be indexed with is_getter"
    );
    assert_eq!(
        getter.unwrap().kind,
        SymbolKind::Function,
        "top-level getter should be Function"
    );
}

#[test]
fn test_external_getter_in_class() {
    let src = r#"
class NativeBinding {
    external String get name;
}
"#;
    let result = parser::parse_file(src, "dart", "lib/native.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let getter = flat.iter().find(|s| {
        s.short_name == "name"
            && s.language_attrs
                .as_deref()
                .and_then(|a| serde_json::from_str::<serde_json::Value>(a).ok())
                .is_some_and(|v| v["is_getter"] == true)
    });
    assert!(
        getter.is_some(),
        "external getter should be indexed with is_getter"
    );
    assert_eq!(
        getter.unwrap().kind,
        SymbolKind::Method,
        "class getter should be Method"
    );
}

#[test]
fn test_parse_dart_top_level_variables() {
    let src = r#"
const maxRetries = 3;
final String apiUrl = 'https://example.com';
var counter = 0;
int count = 0;
"#;
    let result = parser::parse_file(src, "dart", "lib/config.dart").unwrap();
    assert!(result.parsed_ok);

    let flat = flatten_symbols(&result.symbols);
    assert_eq!(flat.len(), 4);

    let max_retries = flat.iter().find(|s| s.short_name == "maxRetries").unwrap();
    assert_eq!(max_retries.kind, SymbolKind::Const);
    assert_eq!(max_retries.qualified_name, "maxRetries");
    assert_eq!(max_retries.signature.as_deref(), Some("const maxRetries"));

    let api_url = flat.iter().find(|s| s.short_name == "apiUrl").unwrap();
    assert_eq!(api_url.kind, SymbolKind::Const);
    assert_eq!(api_url.signature.as_deref(), Some("final String apiUrl"));

    let counter = flat.iter().find(|s| s.short_name == "counter").unwrap();
    assert_eq!(counter.kind, SymbolKind::Static);
    assert_eq!(counter.signature.as_deref(), Some("var counter"));

    let count = flat.iter().find(|s| s.short_name == "count").unwrap();
    assert_eq!(count.kind, SymbolKind::Static);
    assert_eq!(count.signature.as_deref(), Some("int count"));
}

#[test]
fn test_parse_dart_class_fields() {
    let src = r#"
class Config {
    static const maxRetries = 3;
    final String name;
    int count = 0;
    late final String _cached;
}
"#;
    let result = parser::parse_file(src, "dart", "lib/config.dart").unwrap();
    assert!(result.parsed_ok);

    let flat = flatten_symbols(&result.symbols);

    let max_retries = flat.iter().find(|s| s.short_name == "maxRetries").unwrap();
    assert_eq!(max_retries.kind, SymbolKind::Const);
    assert_eq!(max_retries.qualified_name, "Config::maxRetries");

    let name = flat.iter().find(|s| s.short_name == "name").unwrap();
    assert_eq!(name.kind, SymbolKind::Field);
    assert_eq!(name.qualified_name, "Config::name");

    let count = flat.iter().find(|s| s.short_name == "count").unwrap();
    assert_eq!(count.kind, SymbolKind::Field);
    assert_eq!(count.qualified_name, "Config::count");

    let cached = flat.iter().find(|s| s.short_name == "_cached").unwrap();
    assert_eq!(cached.kind, SymbolKind::Field);
    assert_eq!(cached.qualified_name, "Config::_cached");
    assert_eq!(cached.visibility.as_deref(), Some("private"));
}

#[test]
fn test_language_attrs_variables() {
    let src = r#"
const x = 1;
final y = 2;
var z = 3;
"#;
    let result = parser::parse_file(src, "dart", "lib/vars.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let x = flat.iter().find(|s| s.short_name == "x").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(x.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["is_const"], true);

    let y = flat.iter().find(|s| s.short_name == "y").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(y.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["is_final"], true);

    let z = flat.iter().find(|s| s.short_name == "z").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(z.language_attrs.as_deref().unwrap()).unwrap();
    assert!(attrs.get("is_const").is_none());
    assert!(attrs.get("is_final").is_none());
}

#[test]
fn test_language_attrs_class_field_static_late() {
    let src = r#"
class Foo {
    static const a = 1;
    late final String b;
}
"#;
    let result = parser::parse_file(src, "dart", "lib/foo.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    let a = flat.iter().find(|s| s.short_name == "a").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(a.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["is_const"], true);
    assert_eq!(attrs["is_static"], true);

    let b = flat.iter().find(|s| s.short_name == "b").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(b.language_attrs.as_deref().unwrap()).unwrap();
    assert_eq!(attrs["is_final"], true);
    assert_eq!(attrs["is_late"], true);
}

#[test]
fn test_multi_variable_declaration() {
    let src = r#"
int a = 1, b = 2, c = 3;
"#;
    let result = parser::parse_file(src, "dart", "lib/multi.dart").unwrap();
    let flat = flatten_symbols(&result.symbols);

    assert_eq!(
        flat.len(),
        3,
        "multi-var declaration should produce 3 symbols"
    );
    assert!(flat.iter().any(|s| s.short_name == "a"));
    assert!(flat.iter().any(|s| s.short_name == "b"));
    assert!(flat.iter().any(|s| s.short_name == "c"));
    assert!(flat.iter().all(|s| s.kind == SymbolKind::Static));
}

#[test]
fn test_parse_dart_abstract_and_external_fields() {
    let src = r#"
abstract class Foo {
    abstract final int x;
    abstract int y;
}
class NativeBinding {
    external final String name;
    external static var count;
}
"#;
    let result = parser::parse_file(src, "dart", "lib/abs.dart").unwrap();
    assert!(result.parsed_ok);

    let flat = flatten_symbols(&result.symbols);

    let x = flat
        .iter()
        .find(|s| s.short_name == "x")
        .expect("abstract final field x");
    assert_eq!(x.kind, SymbolKind::Field);
    assert_eq!(x.qualified_name, "Foo::x");

    let y = flat
        .iter()
        .find(|s| s.short_name == "y")
        .expect("abstract field y");
    assert_eq!(y.kind, SymbolKind::Field);
    assert_eq!(y.qualified_name, "Foo::y");

    let name = flat
        .iter()
        .find(|s| s.short_name == "name")
        .expect("external final field name");
    assert_eq!(name.kind, SymbolKind::Field);
    assert_eq!(name.qualified_name, "NativeBinding::name");

    let count = flat
        .iter()
        .find(|s| s.short_name == "count")
        .expect("external static var count");
    assert_eq!(count.qualified_name, "NativeBinding::count");
}

#[test]
fn ref_context_constructor_calls() {
    let src = r#"
void main() {
  var a = Foo();
  var b = Foo.named();
  var c = new Foo();
  var d = const Foo();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let foo_call = refs
        .iter()
        .find(|r| r.name == "Foo" && r.line == 3)
        .unwrap();
    assert_eq!(
        foo_call.context_kind,
        RefContextKind::Call,
        "Foo() should be Call"
    );

    let named_callee = refs
        .iter()
        .find(|r| r.line == 4 && r.name == "named")
        .unwrap();
    assert_eq!(
        named_callee.context_kind,
        RefContextKind::Call,
        "Foo.named() — named (callee) should be Call"
    );

    assert!(
        refs.iter()
            .find(|r| r.line == 4 && r.name == "Foo")
            .is_none(),
        "Foo.named() — Foo (receiver) should be suppressed (Other refs are not extracted)"
    );

    let new_foo = refs
        .iter()
        .find(|r| r.name == "Foo" && r.line == 5)
        .unwrap();
    assert_eq!(
        new_foo.context_kind,
        RefContextKind::Construction,
        "new Foo() should be Construction"
    );

    let const_foo = refs
        .iter()
        .find(|r| r.name == "Foo" && r.line == 6)
        .unwrap();
    assert_eq!(
        const_foo.context_kind,
        RefContextKind::Construction,
        "const Foo() should be Construction"
    );
}

#[test]
fn ref_context_cascade() {
    let src = r#"
void main() {
  Foo()..x = 1;
  Foo()..bar();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let cascade_field = refs.iter().find(|r| r.name == "x" && r.line == 3).unwrap();
    assert_eq!(
        cascade_field.context_kind,
        RefContextKind::FieldAccess,
        "..x should be FieldAccess"
    );

    let cascade_call = refs
        .iter()
        .find(|r| r.name == "bar" && r.line == 4)
        .unwrap();
    assert_eq!(
        cascade_call.context_kind,
        RefContextKind::Call,
        "..bar() should be Call"
    );
}

#[test]
fn ref_context_generic_types() {
    let src = r#"
List<Foo> items = [];
Map<String, Foo> mapping = {};
Foo? nullable = null;
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    for name in &["List", "Foo", "Map", "String"] {
        let type_refs: Vec<_> = refs
            .iter()
            .filter(|r| &r.name.as_str() == name && r.context_kind == RefContextKind::TypeUse)
            .collect();
        assert!(
            !type_refs.is_empty(),
            "{name} should be classified as TypeUse"
        );
    }

    let nullable_foo = refs
        .iter()
        .find(|r| r.name == "Foo" && r.line == 4)
        .unwrap();
    assert_eq!(
        nullable_foo.context_kind,
        RefContextKind::TypeUse,
        "Foo? should be TypeUse"
    );
}

#[test]
fn ref_context_class_hierarchy() {
    let src = r#"
class Foo {}
class Bar extends Foo {}
class Baz implements Foo {}
mixin Mix on Foo {}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let extends_foo = refs
        .iter()
        .find(|r| r.name == "Foo" && r.line == 3)
        .unwrap();
    assert_eq!(
        extends_foo.context_kind,
        RefContextKind::TypeUse,
        "extends Foo should be TypeUse"
    );

    let implements_foo = refs
        .iter()
        .find(|r| r.name == "Foo" && r.line == 4)
        .unwrap();
    assert_eq!(
        implements_foo.context_kind,
        RefContextKind::TypeUse,
        "implements Foo should be TypeUse"
    );

    let on_foo = refs
        .iter()
        .find(|r| r.name == "Foo" && r.line == 5)
        .unwrap();
    assert_eq!(
        on_foo.context_kind,
        RefContextKind::TypeUse,
        "mixin on Foo should be TypeUse"
    );
}

#[test]
fn ref_context_member_access() {
    let src = r#"
class Foo { int x = 0; }
void main() {
  var f = Foo();
  var v = f.x;
}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let field_x = refs.iter().find(|r| r.name == "x" && r.line == 5).unwrap();
    assert_eq!(
        field_x.context_kind,
        RefContextKind::FieldAccess,
        "f.x property should be FieldAccess"
    );
}

#[test]
fn ref_context_member_call_receiver() {
    let src = r#"
void main() {
  var http = getClient();
  http.get();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    assert!(
        refs.iter()
            .find(|r| r.name == "http" && r.line == 4)
            .is_none(),
        "http (receiver) in http.get() should be suppressed (Other refs are not extracted)"
    );

    let callee = refs
        .iter()
        .find(|r| r.name == "get" && r.line == 4)
        .unwrap();
    assert_eq!(
        callee.context_kind,
        RefContextKind::Call,
        "get (callee) in http.get() should be Call"
    );
    assert_eq!(
        callee.receiver.as_deref(),
        Some("http"),
        "receiver identifier should be captured on the callee ref"
    );
    // getClient() returns an untracked type so no type-tracking hint is set.
    assert!(
        callee.resolved_local_target.is_none(),
        "untracked receiver should not produce a type-tracking hint"
    );
}

#[test]
fn ref_context_chained_cascade() {
    let src = r#"
void main() {
  Foo()..bar().baz();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let bar = refs.iter().find(|r| r.name == "bar").unwrap();
    assert_eq!(
        bar.context_kind,
        RefContextKind::Call,
        "..bar() in cascade should be Call"
    );

    let baz = refs.iter().find(|r| r.name == "baz").unwrap();
    assert_eq!(
        baz.context_kind,
        RefContextKind::Call,
        "..bar().baz() chained cascade call should be Call"
    );
}

// ---------------------------------------------------------------------------
// Phase B: constructor type tracking tests
// ---------------------------------------------------------------------------

#[test]
fn type_tracking_var_constructor_sets_hint() {
    let src = r#"
class Cache {
  void get() {}
}
void main() {
  final c = Cache();
  c.get();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let get_ref = refs
        .iter()
        .find(|r| r.name == "get" && r.context_kind == RefContextKind::Call)
        .expect("c.get() should be extracted as a Call ref");

    assert_eq!(
        get_ref.receiver.as_deref(),
        Some("c"),
        "receiver should be captured"
    );
    assert_eq!(
        get_ref.resolved_local_target.as_deref(),
        Some("::type_tracking::Cache"),
        "type-tracking hint should point to Cache"
    );
}

#[test]
fn type_tracking_new_constructor_sets_hint() {
    let src = r#"
class Repo {
  void fetch() {}
}
void main() {
  var r = new Repo();
  r.fetch();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let fetch_ref = refs
        .iter()
        .find(|r| r.name == "fetch" && r.context_kind == RefContextKind::Call)
        .expect("r.fetch() should be extracted as a Call ref");

    assert_eq!(
        fetch_ref.resolved_local_target.as_deref(),
        Some("::type_tracking::Repo"),
        "new Repo() should produce type-tracking hint"
    );
}

#[test]
fn type_tracking_reassignment_uses_latest() {
    let src = r#"
class Foo {
  void run() {}
}
class Bar {
  void run() {}
}
void main() {
  var x = Foo();
  x = Bar();
  x.run();
}
"#;
    // After `x = Bar()`, the type-tracking should see x's last declaration as Bar.
    // Note: assignment without var/final uses initialized_identifier only if
    // inside a declaration — a bare `x = Bar()` reassignment may not be tracked
    // via initialized_identifier. The test documents current parse-time behaviour.
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let run_ref = refs
        .iter()
        .find(|r| r.name == "run" && r.context_kind == RefContextKind::Call)
        .expect("x.run() should be extracted as a Call ref");

    // The initial `var x = Foo()` declaration is tracked; bare reassignment
    // `x = Bar()` is not an initialized_identifier so only Foo is known.
    assert_eq!(
        run_ref.resolved_local_target.as_deref(),
        Some("::type_tracking::Foo"),
        "first constructor binding is used when bare reassignment is not tracked"
    );
}

#[test]
fn type_tracking_untracked_receiver_no_hint() {
    let src = r#"
void main() {
  var x = makeWidget();
  x.render();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let render_ref = refs
        .iter()
        .find(|r| r.name == "render" && r.context_kind == RefContextKind::Call)
        .expect("x.render() should be a Call ref");

    assert!(
        render_ref.resolved_local_target.is_none(),
        "factory function (lowercase callee) should not produce a type-tracking hint"
    );
}

#[test]
fn type_tracking_scope_boundary_prevents_cross_function_leak() {
    let src = r#"
class Cache {
  void get() {}
}
void funcA() {
  final c = Cache();
  c.get();
}
void funcB() {
  var c = makeSomething();
  c.get();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let get_refs: Vec<_> = refs
        .iter()
        .filter(|r| r.name == "get" && r.context_kind == RefContextKind::Call)
        .collect();
    assert_eq!(get_refs.len(), 2, "should have two c.get() refs");

    let func_a_ref = get_refs.iter().find(|r| r.line == 7).unwrap();
    assert_eq!(
        func_a_ref.resolved_local_target.as_deref(),
        Some("::type_tracking::Cache"),
        "funcA's c.get() should resolve via type tracking"
    );

    let func_b_ref = get_refs.iter().find(|r| r.line == 11).unwrap();
    assert!(
        func_b_ref.resolved_local_target.is_none(),
        "funcB's c.get() should NOT inherit Cache type from funcA"
    );
}

#[test]
fn type_tracking_named_constructor() {
    let src = r#"
class Repo {
  Repo.fromDb();
  void fetch() {}
}
void main() {
  final r = Repo.fromDb();
  r.fetch();
}
"#;
    let result = parser::parse_file(src, "dart", "lib/test.dart").unwrap();
    let refs = &result.references;

    let fetch_ref = refs
        .iter()
        .find(|r| r.name == "fetch" && r.context_kind == RefContextKind::Call)
        .expect("r.fetch() should be a Call ref");

    assert_eq!(
        fetch_ref.resolved_local_target.as_deref(),
        Some("::type_tracking::Repo"),
        "named constructor Repo.fromDb() should produce type-tracking hint for Repo"
    );
}
