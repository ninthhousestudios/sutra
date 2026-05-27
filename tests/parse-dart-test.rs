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
    assert!(dog.language_attrs.is_none(), "non-abstract class should have no attrs");
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
