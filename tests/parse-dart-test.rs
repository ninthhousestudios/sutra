use sutra::parser::{self, flatten_symbols, SymbolKind};

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

    let animal = result.symbols.iter().find(|s| s.short_name == "Animal").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(animal.language_attrs.as_deref().expect("abstract class should have attrs"))
            .unwrap();
    assert_eq!(attrs["is_abstract"], true);

    let dog = result.symbols.iter().find(|s| s.short_name == "Dog").unwrap();
    assert_eq!(dog.language_attrs.as_deref(), Some("{}"), "non-abstract class should have empty attrs");
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
    assert!(!factory_methods.is_empty(), "should detect factory constructor");
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
    let attrs: serde_json::Value =
        serde_json::from_str(generate.language_attrs.as_deref().expect("async* method should have attrs"))
            .unwrap();
    assert_eq!(attrs["is_async"], true, "async* method should be marked is_async");

    let do_stuff = flat.iter().find(|s| s.short_name == "doStuff").unwrap();
    let attrs: serde_json::Value =
        serde_json::from_str(do_stuff.language_attrs.as_deref().expect("async method should have attrs"))
            .unwrap();
    assert_eq!(attrs["is_async"], true, "async method should be marked is_async");

    let sync_method = flat.iter().find(|s| s.short_name == "sync_method").unwrap();
    assert_eq!(sync_method.language_attrs.as_deref(), Some("{}"), "sync method should have empty attrs");
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
    assert!(do_stuff.is_some(), "static method doStuff should be indexed");

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
    assert_eq!(attrs["is_constructor"], true, "should be marked is_constructor");
}
