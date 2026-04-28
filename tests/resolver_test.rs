use sutra::parser::{
    ExtractedImport, ExtractedRef, ExtractedSymbol, RefContextKind, SymbolKind,
};
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
        parent_qualified_name: None,
        docstring: None,
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

/// Test 1: A local binding `let x = 1` and a ref to `x` resolves locally.
#[test]
fn test_resolve_local_binding() {
    let file_symbols = vec![make_symbol("main::x", "x", SymbolKind::Const, 5, 5)];
    let refs = vec![make_ref("x", 10, RefContextKind::Other)];
    // The symbol is also in all_symbols (as it would be after DB insert).
    let all_symbols = vec![(1_i64, "main::x".to_string(), "x".to_string())];
    let imports: Vec<ExtractedImport> = vec![];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].target_symbol_id, Some(1));
    assert!(resolved[0].unresolved_name.is_none());
}

/// Test 2: Symbol in file A imported by file B, ref in file B resolves via import.
#[test]
fn test_resolve_cross_file() {
    // File B has no local symbol named `Config`, but imports it.
    let file_symbols: Vec<ExtractedSymbol> = vec![];
    let refs = vec![make_ref("Config", 15, RefContextKind::TypeUse)];
    let all_symbols = vec![
        (10_i64, "config::Config".to_string(), "Config".to_string()),
        (20_i64, "other::OtherStruct".to_string(), "OtherStruct".to_string()),
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
        (100_i64, "my_errors::Error".to_string(), "Error".to_string()),
        (200_i64, "other_errors::Error".to_string(), "Error".to_string()),
    ];
    // Import specifically brings in my_errors::Error.
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
    // No symbol named HashMap in the workspace.
    let all_symbols: Vec<(i64, String, String)> = vec![
        (1_i64, "main::main".to_string(), "main".to_string()),
    ];
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
        make_symbol("main::x", "x", SymbolKind::Const, 3, 3),  // outer
        make_symbol("main::inner::x", "x", SymbolKind::Const, 10, 10), // inner
    ];
    let refs = vec![make_ref("x", 12, RefContextKind::Other)]; // after inner
    let all_symbols = vec![
        (1_i64, "main::x".to_string(), "x".to_string()),
        (2_i64, "main::inner::x".to_string(), "x".to_string()),
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
        (42_i64, "ui::Widget".to_string(), "Widget".to_string()),
        (43_i64, "ui::Button".to_string(), "Button".to_string()),
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
        (10_i64, "ast::expr::deep::Node".to_string(), "Node".to_string()),
        (20_i64, "ast::Node".to_string(), "Node".to_string()),
        (30_i64, "ast::stmt::Node".to_string(), "Node".to_string()),
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
    let all_symbols = vec![
        (55_i64, "config::Config".to_string(), "Config".to_string()),
    ];
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
    let all_symbols = vec![
        (77_i64, "models::db::User".to_string(), "User".to_string()),
    ];
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
        (11_i64, "logging::Logger".to_string(), "Logger".to_string()),
        (22_i64, "tracing::Logger".to_string(), "Logger".to_string()),
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
    let file_symbols = vec![make_symbol("local::Config", "Config", SymbolKind::Struct, 2, 10)];
    let refs = vec![make_ref("Config", 8, RefContextKind::TypeUse)];
    let all_symbols = vec![
        (1_i64, "local::Config".to_string(), "Config".to_string()),
        (2_i64, "remote::Config".to_string(), "Config".to_string()),
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
    let all_symbols = vec![
        (99_i64, "myapp::models::MyModel".to_string(), "MyModel".to_string()),
    ];
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
    let all_symbols = vec![(1_i64, "mod::Foo".to_string(), "Foo".to_string())];
    let imports = vec![make_import("mod::Foo", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

    assert!(resolved.is_empty());
}

#[test]
fn test_multiple_refs_mixed_resolution() {
    let file_symbols = vec![make_symbol("app::handler", "handler", SymbolKind::Function, 1, 20)];
    let refs = vec![
        make_ref("handler", 10, RefContextKind::Call),
        make_ref("Config", 12, RefContextKind::TypeUse),
        make_ref("UnknownThing", 14, RefContextKind::Other),
    ];
    let all_symbols = vec![
        (1_i64, "app::handler".to_string(), "handler".to_string()),
        (2_i64, "config::Config".to_string(), "Config".to_string()),
    ];
    let imports = vec![make_import("config::Config", 1)];

    let resolved = resolve_refs(&file_symbols, &refs, &all_symbols, &imports);

    assert_eq!(resolved.len(), 3);
    assert_eq!(resolved[0].target_symbol_id, Some(1), "local 'handler' should resolve");
    assert_eq!(resolved[1].target_symbol_id, Some(2), "imported 'Config' should resolve");
    assert!(resolved[2].target_symbol_id.is_none(), "unknown ref should be unresolved");
    assert_eq!(resolved[2].unresolved_name.as_deref(), Some("UnknownThing"));
}
