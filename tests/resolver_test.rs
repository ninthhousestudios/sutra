use sutra::parser::{ExtractedImport, ExtractedRef, ExtractedSymbol, RefContextKind, SymbolKind};
use sutra::resolver::resolve_refs;

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
        visibility: None,
        start_line,
        start_col: 0,
        end_line,
        end_col: 0,
        children: vec![],
        docstring: None,
        cyclomatic: None,
        cognitive: None,
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
    }
}

fn make_import(raw_path: &str, line: usize) -> ExtractedImport {
    ExtractedImport {
        raw_path: raw_path.to_string(),
        line,
    }
}

fn sym4(id: i64, qn: &str, sn: &str, kind: &str) -> (i64, String, String, String) {
    (id, qn.to_string(), sn.to_string(), kind.to_string())
}

/// Test 1: A local binding `let x = 1` and a ref to `x` resolves locally.
#[test]
fn test_resolve_local_binding() {
    let file_symbols = vec![make_symbol("main::x", "x", SymbolKind::Const, 5, 5)];
    let refs = vec![make_ref("x", 10, RefContextKind::Other)];
    let all_symbols = vec![sym4(1, "main::x", "x", "const")];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
        sym4(10, "config::Config", "Config", "struct"),
        sym4(20, "other::OtherStruct", "OtherStruct", "struct"),
    ];
    let imports = vec![make_import("config::Config", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
        sym4(100, "my_errors::Error", "Error", "struct"),
        sym4(200, "other_errors::Error", "Error", "struct"),
    ];
    let imports = vec![make_import("my_errors::Error", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
    let all_symbols = vec![sym4(1, "main::main", "main", "function")];
    let imports = vec![make_import("std::collections::HashMap", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
        sym4(1, "main::x", "x", "const"),
        sym4(2, "main::inner::x", "x", "const"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
        sym4(42, "ui::Widget", "Widget", "struct"),
        sym4(43, "ui::Button", "Button", "struct"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].target_symbol_id, Some(42));
    assert!(resolved[0].unresolved_name.is_none());
}

#[test]
fn test_global_ambiguous_shortest_qn() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Node", 5, RefContextKind::TypeUse)];
    let all_symbols = vec![
        sym4(10, "ast::expr::deep::Node", "Node", "struct"),
        sym4(20, "ast::Node", "Node", "struct"),
        sym4(30, "ast::stmt::Node", "Node", "struct"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
    let all_symbols = vec![sym4(55, "config::Config", "Config", "struct")];
    let imports = vec![make_import("config::Config", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
    let all_symbols = vec![sym4(77, "models::db::User", "User", "struct")];
    let imports = vec![make_import("models::User", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
        sym4(11, "logging::Logger", "Logger", "struct"),
        sym4(22, "tracing::Logger", "Logger", "struct"),
    ];
    let imports = vec![
        make_import("logging::Logger", 1),
        make_import("tracing::Logger", 2),
    ];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
        sym4(1, "local::Config", "Config", "struct"),
        sym4(2, "remote::Config", "Config", "struct"),
    ];
    let imports = vec![make_import("remote::Config", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
    let all_symbols = vec![sym4(99, "myapp::models::MyModel", "MyModel", "class")];
    let imports = vec![make_import("package:myapp/models.dart", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
    let all_symbols = vec![sym4(1, "mod::Foo", "Foo", "struct")];
    let imports = vec![make_import("mod::Foo", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
        sym4(1, "app::handler", "handler", "function"),
        sym4(2, "config::Config", "Config", "struct"),
    ];
    let imports = vec![make_import("config::Config", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
        sym4(1, "app::Config", "Config", "function"),
        sym4(2, "app::types::Config", "Config", "struct"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
        sym4(1, "app::new_config", "new_config", "struct"),
        sym4(2, "app::factory::new_config", "new_config", "function"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
    let all_symbols = vec![sym4(1, "main::main", "main", "function")];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

    assert_eq!(resolved.len(), 1);
    assert!(resolved[0].skipped, "Import refs should be skipped");
    assert!(resolved[0].target_symbol_id.is_none());
    assert!(resolved[0].unresolved_name.is_none());
}

/// FieldAccess refs should be skipped (need type info).
#[test]
fn test_field_access_skipped() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("len", 10, RefContextKind::FieldAccess)];
    let all_symbols = vec![sym4(1, "vec::len", "len", "method")];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

    assert_eq!(resolved.len(), 1);
    assert!(resolved[0].skipped, "FieldAccess refs should be skipped");
}

/// Kind filter fallback: if no kind-compatible match, fall back to any match.
#[test]
fn test_kind_filter_fallback() {
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Config", 10, RefContextKind::Call)];
    // Only a struct named Config exists — no callable. Fallback should still resolve.
    let all_symbols = vec![sym4(1, "app::Config", "Config", "struct")];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

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
        sym4(1, "Config", "Config", "struct"),
        sym4(2, "Config", "Config", "function"),
    ];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

    assert_eq!(resolved.len(), 1);
    assert_eq!(
        resolved[0].target_symbol_id,
        Some(1),
        "Construction ref should resolve to struct, not function"
    );
}
