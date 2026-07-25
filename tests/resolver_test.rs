use sutra::db::SymbolEntry;
use sutra::parser::rust::LOCAL_BINDING_SENTINEL;
use sutra::parser::{ExtractedImport, ExtractedRef, ExtractedSymbol, RefContextKind, SymbolKind};
use sutra::resolver::{self, ResolutionMethod, resolve_refs};

fn resolve(
    file_symbols: &[ExtractedSymbol],
    refs: &[ExtractedRef],
    all_symbols: &[SymbolEntry],
    imports: &[ExtractedImport],
    file_id: i64,
) -> Vec<resolver::ResolvedRef> {
    let cm = resolver::build_class_members(all_symbols);
    resolve_refs(file_symbols, refs, all_symbols, imports, file_id, &cm)
}

fn make_symbol(
    qualified_name: &str,
    short_name: &str,
    kind: SymbolKind,
    start_line: usize,
    end_line: usize,
) -> ExtractedSymbol {
    ExtractedSymbol {
        qualified_name: qualified_name.to_string(),
        short_name: short_name.to_string(),
        kind,
        signature: None,
        signature_hash: None,
        structural_hash: None,
        visibility: None,
        start_line,
        start_col: 0,
        end_line,
        end_col: 0,
        children: vec![],
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    }
}

fn make_ref(name: &str, line: usize, context_kind: RefContextKind) -> ExtractedRef {
    ExtractedRef {
        name: name.to_string(),
        line,
        col: 0,
        context_kind,
        resolved_local_target: None,
        receiver: None,
    }
}

fn make_import(raw_path: &str, line: usize) -> ExtractedImport {
    ExtractedImport {
        raw_path: raw_path.to_string(),
        line,
        kind: "use",
        alias: None,
        is_test: false,
    }
}

fn sym(id: i64, qn: &str, sn: &str, kind: &str) -> SymbolEntry {
    SymbolEntry {
        id,
        qualified_name: qn.to_string(),
        short_name: sn.to_string(),
        kind: kind.to_string(),
        parent_symbol_id: None,
        file_id: 0,
    }
}

fn sym_in_file(id: i64, qn: &str, sn: &str, kind: &str, file_id: i64) -> SymbolEntry {
    SymbolEntry {
        id,
        qualified_name: qn.to_string(),
        short_name: sn.to_string(),
        kind: kind.to_string(),
        parent_symbol_id: None,
        file_id,
    }
}

/// Test 1: A local binding `let x = 1` and a ref to `x` resolves locally.
#[test]
fn test_resolve_local_binding() {
    let file_symbols = vec![make_symbol("main::x", "x", SymbolKind::Const, 5, 5)];
    let refs = vec![make_ref("x", 10, RefContextKind::Other)];
    let all_symbols = vec![sym(1, "main::x", "x", "const")];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].target_symbol_id, Some(1));
    assert!(resolved[0].unresolved_name.is_none());
}

/// Test 2: Symbol in file A imported by file B, ref in file B resolves via import.
#[test]
fn test_resolve_cross_file() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Config", 15, RefContextKind::TypeUse)];
    let all_symbols = vec![
        sym(10, "config::Config", "Config", "struct"),
        sym(20, "other::OtherStruct", "OtherStruct", "struct"),
    ];
    let imports = vec![make_import("config::Config", 1)];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].target_symbol_id, Some(10));
    assert!(resolved[0].unresolved_name.is_none());
}

/// Test 3: Two symbols with same short_name in different files — picks the imported one.
#[test]
fn test_resolve_ambiguous() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Error", 20, RefContextKind::TypeUse)];
    let all_symbols = vec![
        sym(100, "my_errors::Error", "Error", "struct"),
        sym(200, "other_errors::Error", "Error", "struct"),
    ];
    let imports = vec![make_import("my_errors::Error", 1)];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(100),
        "should resolve to the imported Error, not the other one"
    );
}

/// Test 4: Ref to `HashMap` (stdlib) with no matching symbol → unresolved.
#[test]
fn test_unresolved_external() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("HashMap", 30, RefContextKind::TypeUse)];
    let all_symbols = vec![sym(1, "main::main", "main", "function")];
    let imports = vec![make_import("std::collections::HashMap", 1)];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert!(
        resolved[0].target_symbol_id.is_none(),
        "stdlib symbol should be unresolved"
    );
    assert_eq!(
        resolved[0].unresolved_name.as_deref(),
        Some("HashMap"),
        "unresolved_name should be populated"
    );
}

/// Test 5: Inner scope variable shadows outer — resolves to inner (nearest by line).
#[test]
fn test_scope_shadowing() {
    let file_symbols = vec![
        make_symbol("main::x", "x", SymbolKind::Const, 3, 3), // outer
        make_symbol("main::inner::x", "x", SymbolKind::Const, 10, 10), // inner
    ];
    let refs = vec![make_ref("x", 12, RefContextKind::Other)]; // after inner
    let all_symbols = vec![
        sym(1, "main::x", "x", "const"),
        sym(2, "main::inner::x", "x", "const"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(2),
        "should resolve to the inner (shadowing) symbol, not the outer"
    );
}

#[test]
fn test_global_unique_match() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Widget", 5, RefContextKind::TypeUse)];
    let all_symbols = vec![
        sym(42, "ui::Widget", "Widget", "struct"),
        sym(43, "ui::Button", "Button", "struct"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].target_symbol_id, Some(42));
    assert!(resolved[0].unresolved_name.is_none());
}

#[test]
fn test_global_ambiguous_shortest_qn() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Node", 5, RefContextKind::TypeUse)];
    let all_symbols = vec![
        sym(10, "ast::expr::deep::Node", "Node", "struct"),
        sym(20, "ast::Node", "Node", "struct"),
        sym(30, "ast::stmt::Node", "Node", "struct"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(20),
        "should pick the symbol with the shortest qualified_name"
    );
}

#[test]
fn test_import_chain_qualified_name() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Config", 10, RefContextKind::TypeUse)];
    let all_symbols = vec![sym(55, "config::Config", "Config", "struct")];
    let imports = vec![make_import("config::Config", 1)];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(55),
        "import path matches qualified_name exactly"
    );
}

#[test]
fn test_import_prefix_match() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("User", 10, RefContextKind::TypeUse)];
    let all_symbols = vec![sym(77, "models::db::User", "User", "struct")];
    let imports = vec![make_import("models::User", 1)];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(77),
        "import prefix 'models' should match qualified_name 'models::db::User'"
    );
}

#[test]
fn test_multiple_imports_first_match_wins() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Logger", 20, RefContextKind::TypeUse)];
    let all_symbols = vec![
        sym(11, "logging::Logger", "Logger", "struct"),
        sym(22, "tracing::Logger", "Logger", "struct"),
    ];
    let imports = vec![
        make_import("logging::Logger", 1),
        make_import("tracing::Logger", 2),
    ];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(11),
        "first matching import should win"
    );
}

#[test]
fn test_local_over_import() {
    let file_symbols = vec![make_symbol(
        "local::Config",
        "Config",
        SymbolKind::Struct,
        2,
        10,
    )];
    let refs = vec![make_ref("Config", 8, RefContextKind::TypeUse)];
    let all_symbols = vec![
        sym(1, "local::Config", "Config", "struct"),
        sym(2, "remote::Config", "Config", "struct"),
    ];
    let imports = vec![make_import("remote::Config", 1)];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(1),
        "local symbol should win over imported symbol"
    );
}

#[test]
fn test_dart_package_import() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("MyModel", 15, RefContextKind::TypeUse)];
    let all_symbols = vec![sym(99, "myapp::models::MyModel", "MyModel", "class")];
    let imports = vec![make_import("package:myapp/models.dart", 1)];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(99),
        "dart package import doesn't match via segments, but unique global match resolves it"
    );
}

#[test]
fn test_empty_refs() {
    let file_symbols = vec![make_symbol("mod::Foo", "Foo", SymbolKind::Struct, 1, 5)];
    let refs: Vec<ExtractedRef> = vec![];
    let all_symbols = vec![sym(1, "mod::Foo", "Foo", "struct")];
    let imports = vec![make_import("mod::Foo", 1)];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert!(resolved.is_empty());
}

#[test]
fn test_multiple_refs_mixed_resolution() {
    let file_symbols = vec![make_symbol(
        "app::handler",
        "handler",
        SymbolKind::Function,
        1,
        20,
    )];
    let refs = vec![
        make_ref("handler", 10, RefContextKind::Call),
        make_ref("Config", 12, RefContextKind::TypeUse),
        make_ref("UnknownThing", 14, RefContextKind::Other),
    ];
    let all_symbols = vec![
        sym(1, "app::handler", "handler", "function"),
        sym(2, "config::Config", "Config", "struct"),
    ];
    let imports = vec![make_import("config::Config", 1)];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 3);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(1),
        "local 'handler' should resolve"
    );
    assert_eq!(
        resolved[1].target_symbol_id,
        Some(2),
        "imported 'Config' should resolve"
    );
    assert!(
        resolved[2].target_symbol_id.is_none(),
        "unknown ref should be unresolved"
    );
    assert_eq!(resolved[2].unresolved_name.as_deref(), Some("UnknownThing"));
}

// --- New tests for kind-aware resolution ---

/// TypeUse ref should match struct, not function with the same name.
#[test]
fn test_kind_aware_type_use_prefers_struct() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Config", 10, RefContextKind::TypeUse)];
    let all_symbols = vec![
        sym(1, "app::Config", "Config", "function"),
        sym(2, "app::types::Config", "Config", "struct"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(2),
        "TypeUse should prefer struct over function"
    );
}

/// Call ref should match function, not struct with the same name.
#[test]
fn test_kind_aware_call_prefers_function() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("new_config", 10, RefContextKind::Call)];
    let all_symbols = vec![
        sym(1, "app::new_config", "new_config", "struct"),
        sym(2, "app::factory::new_config", "new_config", "function"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(2),
        "Call should prefer function over struct"
    );
}

/// Import-context refs should be skipped (not counted as unresolved).
#[test]
fn test_import_refs_skipped() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("HashMap", 1, RefContextKind::Import)];
    let all_symbols = vec![sym(1, "main::main", "main", "function")];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert!(resolved[0].skipped, "Import refs should be skipped");
    assert!(resolved[0].target_symbol_id.is_none());
    assert_eq!(resolved[0].unresolved_name.as_deref(), Some("HashMap"));
}

/// FieldAccess refs resolve against field/method symbols via kind_compatible.
#[test]
fn test_field_access_resolves_to_method() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("len", 10, RefContextKind::FieldAccess)];
    let all_symbols = vec![sym(1, "vec::len", "len", "method")];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert!(
        !resolved[0].skipped,
        "FieldAccess refs should resolve, not skip"
    );
    assert_eq!(resolved[0].target_symbol_id, Some(1));
}

/// Kind filter fallback: if no kind-compatible match, fall back to any match.
#[test]
fn test_kind_filter_fallback() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Config", 10, RefContextKind::Call)];
    // Only a struct named Config exists — no callable. Fallback should still resolve.
    let all_symbols = vec![sym(1, "app::Config", "Config", "struct")];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(1),
        "should fall back to struct when no function matches a Call ref"
    );
}

#[test]
fn test_construction_prefers_struct_over_function() {
    let file_symbols = vec![];
    let refs = vec![make_ref("Config", 10, RefContextKind::Construction)];
    let all_symbols = vec![
        sym(1, "Config", "Config", "struct"),
        sym(2, "Config", "Config", "function"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(1),
        "Construction ref should resolve to struct, not function"
    );
}

// --- Nested-scope tests (Phase 0: enclosing-scope preference) ---

/// Method in impl vs same-named free function — ref inside the impl should
/// resolve to the impl method, not the free function.
#[test]
fn test_scope_impl_method_over_free_function() {
    // impl Foo (lines 1-20) { fn bar (3-5); fn baz (7-19) { bar(); } }
    // fn bar (22-25)  — free function
    let file_symbols = vec![
        make_symbol("Foo", "Foo", SymbolKind::Struct, 1, 20),
        make_symbol("Foo::bar", "bar", SymbolKind::Function, 3, 5),
        make_symbol("Foo::baz", "baz", SymbolKind::Function, 7, 19),
        make_symbol("bar", "bar", SymbolKind::Function, 22, 25),
    ];
    let refs = vec![make_ref("bar", 10, RefContextKind::Call)];
    let all_symbols = vec![
        sym(1, "Foo", "Foo", "struct"),
        sym(2, "Foo::bar", "bar", "function"),
        sym(3, "Foo::baz", "baz", "function"),
        sym(4, "bar", "bar", "function"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(2),
        "ref inside impl Foo should resolve to Foo::bar, not free fn bar"
    );
}

/// Inner fn shadows outer name — ref after the inner definition should
/// resolve to the inner fn.
#[test]
fn test_scope_inner_fn_shadows_outer() {
    // fn outer (1-30) { fn helper (3-8); fn inner (10-28) { fn helper (12-17); helper(); } }
    let file_symbols = vec![
        make_symbol("outer", "outer", SymbolKind::Function, 1, 30),
        make_symbol("outer::helper", "helper", SymbolKind::Function, 3, 8),
        make_symbol("outer::inner", "inner", SymbolKind::Function, 10, 28),
        make_symbol(
            "outer::inner::helper",
            "helper",
            SymbolKind::Function,
            12,
            17,
        ),
    ];
    let refs = vec![make_ref("helper", 20, RefContextKind::Call)];
    let all_symbols = vec![
        sym(1, "outer", "outer", "function"),
        sym(2, "outer::helper", "helper", "function"),
        sym(3, "outer::inner", "inner", "function"),
        sym(4, "outer::inner::helper", "helper", "function"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(4),
        "ref inside inner should resolve to inner::helper, not outer::helper"
    );
}

/// Global fallback prefers same-file candidate when multiple files define the
/// same short_name.
#[test]
fn test_global_fallback_same_file_preference() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("process", 10, RefContextKind::Call)];
    let all_symbols = vec![
        sym_in_file(1, "other::process", "process", "function", 99),
        sym_in_file(2, "this::process", "process", "function", 42),
        sym_in_file(3, "third::process", "process", "function", 77),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 42);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(2),
        "global fallback should prefer the candidate in the same file (file_id=42)"
    );
}

/// Local resolution must bind to the same-file DB row, not the first global
/// match. Regression: duplicate qualified_names across files (e.g. C static
/// functions) would resolve to whichever row appeared first in the DB.
#[test]
fn test_local_binds_to_same_file_row() {
    // Two files both define "helper". The ref is in file 42.
    let file_symbols = vec![make_symbol("helper", "helper", SymbolKind::Function, 5, 10)];
    let refs = vec![make_ref("helper", 8, RefContextKind::Call)];
    let all_symbols = vec![
        sym_in_file(99, "helper", "helper", "function", 77), // other file, appears first
        sym_in_file(42, "helper", "helper", "function", 42), // same file
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 42);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(42),
        "local resolution must bind to the same-file DB row, not the first global match"
    );
}

/// Scope ranking must not drop an enclosing scope that shares the candidate's
/// line range but is a different symbol. Regression: single-line impl + method
/// at the same range caused the impl to be excluded as "self".
#[test]
fn test_scope_same_line_range_different_symbols() {
    // Two single-line symbols at line 5: a struct and a const inside a module (1-10).
    // A ref at line 8 should prefer the const inside the module over an outer const.
    let file_symbols = vec![
        make_symbol("mod_a", "mod_a", SymbolKind::Module, 1, 10),
        make_symbol("mod_a::FOO", "FOO", SymbolKind::Const, 5, 5),
        make_symbol("FOO", "FOO", SymbolKind::Const, 15, 15),
    ];
    let refs = vec![make_ref("FOO", 8, RefContextKind::Other)];
    let all_symbols = vec![
        sym(1, "mod_a", "mod_a", "module"),
        sym(2, "mod_a::FOO", "FOO", "const"),
        sym(3, "FOO", "FOO", "const"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(2),
        "should prefer mod_a::FOO (inside enclosing module) over top-level FOO"
    );
}

// ---------------------------------------------------------------
// Scope-chain hint tests (Phase A)
// ---------------------------------------------------------------

fn make_ref_with_hint(
    name: &str,
    line: usize,
    context_kind: RefContextKind,
    hint: &str,
) -> ExtractedRef {
    ExtractedRef {
        name: name.to_string(),
        line,
        col: 0,
        context_kind,
        resolved_local_target: Some(hint.to_string()),
        receiver: None,
    }
}

#[test]
fn test_scope_hint_short_circuits_resolution() {
    let file_syms = vec![
        make_symbol("process", "process", SymbolKind::Function, 1, 3),
        make_symbol("Foo::process", "process", SymbolKind::Method, 5, 7),
    ];
    let all_syms = vec![
        sym_in_file(1, "process", "process", "function", 10),
        sym_in_file(2, "Foo::process", "process", "method", 10),
    ];
    // Ref with a scope hint pointing to Foo::process
    let refs = vec![make_ref_with_hint(
        "process",
        6,
        RefContextKind::Call,
        "Foo::process",
    )];
    let resolved = resolve(&file_syms, &refs, &all_syms, &[], 10);
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(2),
        "scope hint should direct resolution to Foo::process"
    );
    assert_eq!(
        resolved[0].resolution_method,
        Some(ResolutionMethod::ScopeChain)
    );
}

#[test]
fn test_local_binding_hint_suppresses_resolution() {
    let file_syms = vec![make_symbol("config", "config", SymbolKind::Function, 1, 3)];
    let all_syms = vec![sym_in_file(1, "config", "config", "function", 10)];
    let refs = vec![make_ref_with_hint(
        "config",
        6,
        RefContextKind::Call,
        LOCAL_BINDING_SENTINEL,
    )];
    let resolved = resolve(&file_syms, &refs, &all_syms, &[], 10);
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id, None,
        "local binding sentinel should prevent cross-file resolution"
    );
    assert!(!resolved[0].skipped);
    assert_eq!(
        resolved[0].resolution_method,
        Some(ResolutionMethod::LocalBinding)
    );
}

#[test]
fn test_resolution_method_import() {
    let file_syms = vec![];
    let all_syms = vec![sym(1, "crate::util::process", "process", "function")];
    let imports = vec![make_import("crate::util::process", 1)];
    let refs = vec![make_ref("process", 5, RefContextKind::Call)];
    let resolved = resolve(&file_syms, &refs, &all_syms, &imports, 99);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].target_symbol_id, Some(1));
    assert_eq!(
        resolved[0].resolution_method,
        Some(ResolutionMethod::Import)
    );
}

#[test]
fn test_resolution_method_global_fallback() {
    let file_syms = vec![];
    let all_syms = vec![sym(1, "other::Widget", "Widget", "struct")];
    let refs = vec![make_ref("Widget", 5, RefContextKind::TypeUse)];
    let resolved = resolve(&file_syms, &refs, &all_syms, &[], 99);
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].target_symbol_id, Some(1));
    assert_eq!(
        resolved[0].resolution_method,
        Some(ResolutionMethod::GlobalFallback)
    );
}

#[test]
fn test_scope_hint_fallback_when_db_row_missing() {
    // Hint points to a qualified_name that doesn't exist in all_symbols —
    // should fall through to the normal resolution path.
    let file_syms = vec![make_symbol(
        "process",
        "process",
        SymbolKind::Function,
        1,
        3,
    )];
    let all_syms = vec![sym_in_file(1, "process", "process", "function", 10)];
    let refs = vec![make_ref_with_hint(
        "process",
        2,
        RefContextKind::Call,
        "nonexistent::process",
    )];
    let resolved = resolve(&file_syms, &refs, &all_syms, &[], 10);
    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(1),
        "should fall through to normal local resolution"
    );
    assert_eq!(
        resolved[0].resolution_method,
        Some(ResolutionMethod::ScopeChain),
        "local resolution still uses scope_chain method"
    );
}

// ---------------------------------------------------------------------------
// Phase B: type-tracking resolution tests
// ---------------------------------------------------------------------------

fn sym_with_parent(id: i64, qn: &str, sn: &str, kind: &str, parent_id: i64) -> SymbolEntry {
    SymbolEntry {
        id,
        qualified_name: qn.to_string(),
        short_name: sn.to_string(),
        kind: kind.to_string(),
        parent_symbol_id: Some(parent_id),
        file_id: 0,
    }
}

fn make_ref_with_type_tracking(name: &str, line: usize, class_name: &str) -> ExtractedRef {
    use sutra::parser::dart::TYPE_TRACKING_PREFIX;
    ExtractedRef {
        name: name.to_string(),
        line,
        col: 0,
        context_kind: RefContextKind::Call,
        resolved_local_target: Some(format!("{TYPE_TRACKING_PREFIX}{class_name}")),
        receiver: Some("c".to_string()),
    }
}

/// `final c = Cache(); c.get()` resolves to Cache::get rather than a
/// same-named global function.
#[test]
fn test_type_tracking_resolves_to_class_member_not_global() {
    // Two `get` symbols: a global function and Cache::get (method)
    let all_syms = vec![
        sym(1, "get", "get", "function"),                     // global get
        sym(2, "Cache", "Cache", "class"),                    // the class
        sym_with_parent(3, "Cache::get", "get", "method", 2), // Cache.get
    ];
    let refs = vec![make_ref_with_type_tracking("get", 5, "Cache")];

    let resolved = resolve(&[], &refs, &all_syms, &[], 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(3),
        "should resolve to Cache::get (id=3), not the global get (id=1)"
    );
    assert_eq!(
        resolved[0].resolution_method,
        Some(ResolutionMethod::TypeTracking),
        "resolution method should be TypeTracking"
    );
}

/// When the receiver type is not in all_symbols, fall through to normal
/// resolution (global fallback) without error.
#[test]
fn test_type_tracking_falls_through_when_class_absent() {
    let all_syms = vec![sym(1, "render", "render", "function")];
    let refs = vec![make_ref_with_type_tracking("render", 3, "Widget")];

    let resolved = resolve(&[], &refs, &all_syms, &[], 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(1),
        "should fall through to global function when class is absent"
    );
    assert_ne!(
        resolved[0].resolution_method,
        Some(ResolutionMethod::TypeTracking),
        "should NOT be TypeTracking when class was not found"
    );
}

// ---------------------------------------------------------------------------
// Phase C: import alias resolution
// ---------------------------------------------------------------------------

#[test]
fn test_import_alias_resolves_via_alias_name() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("np", 5, RefContextKind::Call)];
    let all_symbols = vec![sym(42, "numpy", "numpy", "function")];
    let imports = vec![ExtractedImport {
        raw_path: "numpy".to_string(),
        line: 1,
        kind: "import",
        alias: Some("np".to_string()),
        is_test: false,
    }];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(42),
        "`np` should resolve to `numpy` via alias"
    );
    assert_eq!(
        resolved[0].resolution_method,
        Some(ResolutionMethod::Import)
    );
}

#[test]
fn test_dot_separated_import_resolves() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Path", 5, RefContextKind::TypeUse)];
    let all_symbols = vec![sym(42, "pathlib.Path", "Path", "class")];
    let imports = vec![ExtractedImport {
        raw_path: "pathlib.Path".to_string(),
        line: 1,
        kind: "from_import",
        alias: None,
        is_test: false,
    }];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(42),
        "dot-separated Python import should resolve"
    );
}

#[test]
fn test_from_import_alias_resolves() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("OD", 5, RefContextKind::Construction)];
    let all_symbols = vec![sym(42, "collections.OrderedDict", "OrderedDict", "class")];
    let imports = vec![ExtractedImport {
        raw_path: "collections.OrderedDict".to_string(),
        line: 1,
        kind: "from_import",
        alias: Some("OD".to_string()),
        is_test: false,
    }];

    let resolved = resolve(&file_symbols, &refs, &all_symbols, &imports, 0);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(42),
        "`OD` should resolve to `OrderedDict` via from-import alias"
    );
}
