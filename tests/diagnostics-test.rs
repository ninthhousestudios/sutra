use sutra::db::{Db, InsertSymbolParams, ResolveResult};
use sutra::diagnostics::Diagnostic;
use sutra::tools::{calls, find, refs};

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("test", dir.path()).unwrap();
    (dir, db)
}

fn seed_file(db: &Db, path: &str) -> i64 {
    db.upsert_file(path, "rust", "abc123", 100, true).unwrap()
}

fn seed_symbol(db: &Db, file_id: i64, qn: &str, sn: &str, kind: &str) -> i64 {
    db.insert_symbol(&InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind,
        signature: None,
        signature_hash: None,
        visibility: Some("pub"),
        start_line: 1,
        start_col: 0,
        end_line: 10,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        flags: 0,
    })
    .unwrap()
}

// ---------------------------------------------------------------------------
// Diagnostic enum
// ---------------------------------------------------------------------------

#[test]
fn test_suggest_next_query_non_empty() {
    let variants: Vec<Diagnostic> = vec![
        Diagnostic::NoSuchSymbol {
            queried_name: "foo".into(),
            queried_kind: None,
            indexed_kinds: vec![],
            suggestion: "try grep".into(),
        },
        Diagnostic::Ambiguous {
            queried_name: "bar".into(),
            candidates: vec![],
            suggestion: "use qualified name".into(),
        },
        Diagnostic::Stale {
            file: "a.rs".into(),
            staleness_seconds: 60,
            suggestion: "reparse".into(),
        },
        Diagnostic::AnalysisTierDisabled {
            tool: "refs".into(),
            suggestion: "enable analysis".into(),
        },
        Diagnostic::PartialResolution {
            resolved_name: "Foo".into(),
            unresolved_count: 3,
            suggestion: "check imports".into(),
        },
        Diagnostic::SymbolExistsWithNoResults {
            symbol: "Foo::bar".into(),
            symbol_kind: "function".into(),
            tool: "refs".into(),
            suggestion: "may be dead code".into(),
        },
    ];
    for d in &variants {
        assert!(!d.suggest_next_query().is_empty(), "empty suggestion for {d:?}");
    }
}

#[test]
fn test_diagnostic_json_has_kind_tag() {
    let d = Diagnostic::NoSuchSymbol {
        queried_name: "foo".into(),
        queried_kind: None,
        indexed_kinds: vec!["function".into()],
        suggestion: "try grep".into(),
    };
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["kind"], "no_such_symbol");
    assert_eq!(v["queried_name"], "foo");
    assert_eq!(v["indexed_kinds"][0], "function");
}

#[test]
fn test_ambiguous_json_has_candidates() {
    let d = Diagnostic::Ambiguous {
        queried_name: "Config".into(),
        candidates: vec![
            sutra::diagnostics::CandidateInfo {
                qualified_name: "app::Config".into(),
                kind: "struct".into(),
                file: "src/app.rs".into(),
            },
            sutra::diagnostics::CandidateInfo {
                qualified_name: "db::Config".into(),
                kind: "struct".into(),
                file: "src/db.rs".into(),
            },
        ],
        suggestion: "qualify it".into(),
    };
    let v = serde_json::to_value(&d).unwrap();
    assert_eq!(v["kind"], "ambiguous");
    assert_eq!(v["candidates"].as_array().unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// ResolveResult
// ---------------------------------------------------------------------------

#[test]
fn test_resolve_symbol_diagnostic_unique() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_symbol(&db, fid, "MyStruct", "MyStruct", "struct");

    match db.resolve_symbol_diagnostic("MyStruct", None).unwrap() {
        ResolveResult::Unique(s) => assert_eq!(s.qualified_name, "MyStruct"),
        other => panic!("expected Unique, got {other:?}"),
    }
}

#[test]
fn test_resolve_symbol_diagnostic_not_found() {
    let (_dir, db) = setup_db();
    match db.resolve_symbol_diagnostic("ghost", None).unwrap() {
        ResolveResult::NotFound => {}
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn test_resolve_symbol_diagnostic_ambiguous() {
    let (_dir, db) = setup_db();
    let f1 = seed_file(&db, "src/a.rs");
    let f2 = seed_file(&db, "src/b.rs");
    seed_symbol(&db, f1, "a::Config", "Config", "struct");
    seed_symbol(&db, f2, "b::Config", "Config", "struct");

    match db.resolve_symbol_diagnostic("Config", None).unwrap() {
        ResolveResult::Ambiguous(candidates) => {
            assert!(candidates.len() >= 2, "expected >= 2 candidates");
        }
        other => panic!("expected Ambiguous, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tool retrofits
// ---------------------------------------------------------------------------

#[test]
fn test_find_no_match_returns_diagnostic() {
    let (_dir, db) = setup_db();
    let fid = seed_file(&db, "src/lib.rs");
    seed_symbol(&db, fid, "RealThing", "RealThing", "struct");

    let result = find::handle(&db, "nonexistent", None, None).unwrap();
    assert_eq!(result["total"], 0);
    assert_eq!(result["diagnostic"]["kind"], "no_such_symbol");
    assert_eq!(result["diagnostic"]["queried_name"], "nonexistent");
    let kinds = result["diagnostic"]["indexed_kinds"].as_array().unwrap();
    assert!(kinds.iter().any(|k| k == "struct"));
}

#[test]
fn test_refs_not_found_returns_diagnostic() {
    let (_dir, db) = setup_db();
    let result = refs::handle(&db, "ghost", None).unwrap();
    assert_eq!(result["diagnostic"]["kind"], "no_such_symbol");
}

#[test]
fn test_refs_ambiguous_returns_candidates() {
    let (_dir, db) = setup_db();
    let f1 = seed_file(&db, "src/a.rs");
    let f2 = seed_file(&db, "src/b.rs");
    seed_symbol(&db, f1, "a::Handle", "Handle", "struct");
    seed_symbol(&db, f2, "b::Handle", "Handle", "struct");

    let result = refs::handle(&db, "Handle", None).unwrap();
    assert_eq!(result["diagnostic"]["kind"], "ambiguous");
    let candidates = result["diagnostic"]["candidates"].as_array().unwrap();
    assert!(candidates.len() >= 2);
}

#[test]
fn test_calls_not_found_returns_diagnostic() {
    let (_dir, db) = setup_db();
    let result = calls::handle(&db, "ghost", None, None).unwrap();
    assert_eq!(result["diagnostic"]["kind"], "no_such_symbol");
}

#[test]
fn test_calls_ambiguous_returns_candidates() {
    let (_dir, db) = setup_db();
    let f1 = seed_file(&db, "src/a.rs");
    let f2 = seed_file(&db, "src/b.rs");
    seed_symbol(&db, f1, "a::process", "process", "function");
    seed_symbol(&db, f2, "b::process", "process", "function");

    let result = calls::handle(&db, "process", None, None).unwrap();
    assert_eq!(result["diagnostic"]["kind"], "ambiguous");
}
