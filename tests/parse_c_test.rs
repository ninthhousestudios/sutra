use std::collections::HashMap;
use std::path::Path;

use sutra::c_imports::{parse_compile_commands, resolve_quoted_include};
use sutra::parser::RefContextKind;
use sutra::parser::SymbolKind;
use sutra::parser::adapter::default_registry;
use sutra::parser::c::{FLAG_FFI_ENTRY, FLAG_TEST};
use sutra::parser::parse_file;

// -- Step 1: Smoke + symbol extraction --

#[test]
fn smoke_c_function() {
    let src = "int add(int a, int b) { return a + b; }";
    let r = parse_file(src, "c", "test.c").unwrap();
    assert!(r.parsed_ok);
    assert_eq!(r.symbols.len(), 1);
    let sym = &r.symbols[0];
    assert_eq!(sym.kind, SymbolKind::Function);
    assert_eq!(sym.qualified_name, "add");
    assert_eq!(sym.short_name, "add");
    let sig = sym
        .signature
        .as_ref()
        .expect("function should have signature");
    assert!(sig.contains("int add(int a, int b)"), "sig: {sig}");
    assert!(sym.signature_hash.is_some());
}

#[test]
fn struct_extracted() {
    let r = parse_file("struct Point { int x; int y; };", "c", "test.c").unwrap();
    assert!(r.parsed_ok);
    let sym = r
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Struct)
        .expect("should find struct");
    assert_eq!(sym.short_name, "Point");
    assert_eq!(sym.visibility.as_deref(), Some("pub"));
}

#[test]
fn enum_extracted() {
    let r = parse_file("enum Color { RED, GREEN, BLUE };", "c", "test.c").unwrap();
    assert!(r.parsed_ok);
    let sym = r
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Enum)
        .expect("should find enum");
    assert_eq!(sym.short_name, "Color");
}

#[test]
fn typedef_extracted() {
    let r = parse_file("typedef unsigned long size_t;", "c", "test.c").unwrap();
    assert!(r.parsed_ok);
    let sym = r
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::TypeAlias)
        .expect("should find typedef");
    assert_eq!(sym.short_name, "size_t");
    assert_eq!(sym.visibility.as_deref(), Some("pub"));
}

#[test]
fn function_macro_extracted() {
    let src = "#define MAX(a,b) ((a)>(b)?(a):(b))\nvoid dummy(void) {}";
    let r = parse_file(src, "c", "test.c").unwrap();
    let sym = r
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Macro)
        .expect("should find macro");
    assert_eq!(sym.short_name, "MAX");
}

#[test]
fn value_macro_extracted() {
    let src = "#define BUFFER_SIZE 1024\nvoid dummy(void) {}";
    let r = parse_file(src, "c", "test.c").unwrap();
    assert!(r.parsed_ok);
    let sym = r
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Const)
        .expect("should find const");
    assert_eq!(sym.short_name, "BUFFER_SIZE");
}

#[test]
fn global_var_extracted() {
    let r = parse_file("int global_count = 0;", "c", "test.c").unwrap();
    assert!(r.parsed_ok);
    let sym = r
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Static)
        .expect("should find static");
    assert_eq!(sym.short_name, "global_count");
}

#[test]
fn typedef_struct_extracts_both() {
    let r = parse_file("typedef struct Node { int val; } Node;", "c", "test.c").unwrap();
    assert!(r.parsed_ok);
    let structs: Vec<_> = r
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::Struct)
        .collect();
    let aliases: Vec<_> = r
        .symbols
        .iter()
        .filter(|s| s.kind == SymbolKind::TypeAlias)
        .collect();
    assert_eq!(structs.len(), 1, "expected 1 struct, got: {structs:?}");
    assert_eq!(aliases.len(), 1, "expected 1 typedef, got: {aliases:?}");
}

// -- Step 2: Header guard filtering --

#[test]
fn header_guard_filtered() {
    let src =
        "#define FOO_H\n#define MY_HEADER_INCLUDED\n#define MAX_SIZE 1024\nvoid dummy(void) {}";
    let r = parse_file(src, "c", "test.c").unwrap();
    assert!(
        !r.symbols.iter().any(|s| s.short_name == "FOO_H"),
        "FOO_H header guard should be filtered"
    );
    assert!(
        !r.symbols
            .iter()
            .any(|s| s.short_name == "MY_HEADER_INCLUDED"),
        "MY_HEADER_INCLUDED header guard should be filtered"
    );
    let max_size = r
        .symbols
        .iter()
        .find(|s| s.short_name == "MAX_SIZE")
        .expect("MAX_SIZE should be present");
    assert_eq!(max_size.kind, SymbolKind::Const);
}

// -- Step 3: Reference classification --

#[test]
fn reference_classification() {
    let src = r#"
typedef struct { int x; int y; } Point;
void f(Point p) {
    printf("hi");
    p.x = 1;
}
"#;
    let r = parse_file(src, "c", "test.c").unwrap();

    let calls: Vec<_> = r
        .references
        .iter()
        .filter(|r| r.name == "printf" && r.context_kind == RefContextKind::Call)
        .collect();
    assert_eq!(
        calls.len(),
        1,
        "printf should be a Call ref: {:?}",
        r.references
    );

    let field_accesses: Vec<_> = r
        .references
        .iter()
        .filter(|r| r.name == "x" && r.context_kind == RefContextKind::FieldAccess)
        .collect();
    assert_eq!(
        field_accesses.len(),
        1,
        "x should be a FieldAccess ref: {:?}",
        r.references
    );

    let type_uses: Vec<_> = r
        .references
        .iter()
        .filter(|r| r.name == "Point" && r.context_kind == RefContextKind::TypeUse)
        .collect();
    assert!(
        !type_uses.is_empty(),
        "Point should have TypeUse ref: {:?}",
        r.references
    );
}

// -- Step 4: Import edge extraction --

#[test]
fn includes_produce_imports() {
    let src = "#include \"foo.h\"\n#include <stdio.h>\nvoid f() {}";
    let r = parse_file(src, "c", "test.c").unwrap();
    assert_eq!(r.imports.len(), 2, "expected 2 imports: {:?}", r.imports);
    let paths: Vec<&str> = r.imports.iter().map(|i| i.raw_path.as_str()).collect();
    assert!(
        paths.contains(&"\"foo.h\""),
        "missing quoted include: {paths:?}"
    );
    assert!(
        paths.contains(&"<stdio.h>"),
        "missing angle-bracket include: {paths:?}"
    );
}

// -- Step 5: Complexity --

#[test]
fn complexity_scores() {
    let src = r#"
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
    let r = parse_file(src, "c", "test.c").unwrap();
    let sym = &r.symbols[0];
    // base 1 + 2*if + for + while + 2*case - switch + && = 7
    assert_eq!(sym.cyclomatic, Some(7), "cyclomatic");
    assert!(sym.cognitive.unwrap() > 0, "cognitive should be > 0");
    assert!(sym.max_nesting.unwrap() > 0, "max_nesting should be > 0");
}

// -- Step 6: Language attrs --

#[test]
fn language_attrs_full() {
    let src = "static inline int* f(const int* p, ...) { return 0; }";
    let r = parse_file(src, "c", "test.c").unwrap();
    let sym = &r.symbols[0];
    let attrs: serde_json::Value =
        serde_json::from_str(sym.language_attrs.as_deref().expect("should have attrs")).unwrap();
    assert_eq!(attrs["is_static"], true);
    assert_eq!(attrs["is_inline"], true);
    assert_eq!(attrs["returns_ptr"], true);
    assert_eq!(attrs["has_const"], true);
    assert_eq!(attrs["takes_ptr"], true);
    assert_eq!(attrs["is_variadic"], true);
    assert!(
        attrs.get("returns_void").is_none(),
        "returns_void should be absent"
    );
}

#[test]
fn language_attrs_void_return() {
    let src = "void g(void) {}";
    let r = parse_file(src, "c", "test.c").unwrap();
    let sym = &r.symbols[0];
    let attrs: serde_json::Value =
        serde_json::from_str(sym.language_attrs.as_deref().expect("should have attrs")).unwrap();
    assert_eq!(attrs["returns_void"], true);
}

// -- Step 7: Flags --

#[test]
fn test_file_flag() {
    let src = "void test_init(void) {}";
    let r = parse_file(src, "c", "tests/test_foo.c").unwrap();
    let sym = &r.symbols[0];
    assert_ne!(sym.flags & FLAG_TEST, 0, "FLAG_TEST should be set");
}

#[test]
fn ffi_flag() {
    let src = r#"
__attribute__((visibility("default")))
void exported_func(void) {}
"#;
    let r = parse_file(src, "c", "test.c").unwrap();
    let sym = r
        .symbols
        .iter()
        .find(|s| s.kind == SymbolKind::Function)
        .expect("should find function");
    assert_ne!(
        sym.flags & FLAG_FFI_ENTRY,
        0,
        "FLAG_FFI_ENTRY should be set, flags={:#x}",
        sym.flags
    );
}

// -- Step 8: Import resolution --

#[test]
fn resolve_relative_include() {
    let id_to_path: HashMap<i64, &str> = HashMap::from([(1, "src/foo.c"), (2, "src/bar.h")]);
    let path_to_id: HashMap<&str, i64> = HashMap::from([("src/foo.c", 1), ("src/bar.h", 2)]);
    let ws = Path::new("/workspace");

    let resolved = resolve_quoted_include("bar.h", 1, &id_to_path, &path_to_id, ws, &[]);
    assert_eq!(
        resolved,
        Some(2),
        "bar.h should resolve relative to src/foo.c"
    );
}

#[test]
fn resolve_from_workspace_root() {
    let id_to_path: HashMap<i64, &str> = HashMap::from([(1, "src/main.c"), (2, "include/api.h")]);
    let path_to_id: HashMap<&str, i64> = HashMap::from([("src/main.c", 1), ("include/api.h", 2)]);
    let ws = Path::new("/workspace");

    let resolved = resolve_quoted_include("include/api.h", 1, &id_to_path, &path_to_id, ws, &[]);
    assert_eq!(
        resolved,
        Some(2),
        "include/api.h should resolve from workspace root"
    );
}

#[test]
fn system_include_stays_unresolved() {
    let src = "#include <stdio.h>\nvoid f() {}";
    let r = parse_file(src, "c", "test.c").unwrap();
    let import = r
        .imports
        .iter()
        .find(|i| i.raw_path.starts_with('<'))
        .unwrap();
    assert!(
        import.raw_path.starts_with('<'),
        "angle-bracket include should be preserved as-is"
    );
}

#[test]
fn compile_commands_include_paths() {
    let dir = tempfile::tempdir().unwrap();
    let json = r#"[{
        "directory": ".",
        "file": "src/main.c",
        "arguments": ["cc", "-Iinclude", "-isystem", "vendor/include", "-c", "src/main.c"]
    }]"#;
    std::fs::write(dir.path().join("compile_commands.json"), json).unwrap();
    let cc = parse_compile_commands(dir.path());
    assert!(
        cc.all_dirs.iter().any(|d| d.contains("include")),
        "should extract -I dirs: {:?}",
        cc.all_dirs
    );
    assert!(
        cc.all_dirs.iter().any(|d| d.contains("vendor")),
        "should extract -isystem dirs: {:?}",
        cc.all_dirs
    );
}

// -- Step 9: Adapter integration --

#[test]
fn c_adapter_registered() {
    let registry = default_registry();
    let adapter = registry
        .adapter_for_language("c")
        .expect("CAdapter should be registered");
    assert_eq!(adapter.language_id(), "c");
    assert!(adapter.extensions().contains(&"c"), "should handle .c");
    assert!(adapter.extensions().contains(&"h"), "should handle .h");
}

#[test]
fn effect_patterns_present() {
    let registry = default_registry();
    let adapter = registry.adapter_for_language("c").unwrap();
    let fca = adapter.as_fca_source().expect("C should have FCA source");
    let patterns = fca.effect_patterns();
    let names: Vec<_> = patterns.iter().map(|p| p.attr_name).collect();
    assert!(names.contains(&"effect:heap"), "missing heap: {names:?}");
    assert!(names.contains(&"effect:fs"), "missing fs: {names:?}");
    assert!(names.contains(&"effect:io"), "missing io: {names:?}");
    assert!(names.contains(&"effect:net"), "missing net: {names:?}");
}
