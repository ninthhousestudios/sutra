//! C++ adapter. Layered on `c.rs` the way `typescript.rs` layers on
//! `javascript.rs`: the C subset (functions, structs, enums, typedefs, macros,
//! globals, includes, references) goes through the C extractors with
//! [`Dialect::Cpp`], so the two languages cannot drift apart on shared syntax.
//! Design decisions: `docs/adding-languages.md` § C++ (sutra/525).

use crate::error::Result;
use crate::parser::ParseResult;
use crate::parser::adapter::ParseContext;
use crate::parser::c::{self, Dialect};

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
}
