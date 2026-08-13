use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use sutra::config::Config;
use sutra::db::{Db, SnapshotParams};
use sutra::parser::adapter::default_registry;
use sutra::parser::{RefContextKind, SymbolKind};
use sutra::pipeline;
use sutra::workspace::WorkspaceEntry;

fn make_config(db_dir: &std::path::Path) -> Config {
    Config {
        db_dir: db_dir.to_path_buf(),
        workspaces_path: db_dir.join("workspaces.toml"),
        listen_addr: "127.0.0.1:0".to_string(),
        parse_parallelism: 1,
        stale_threshold_sec: 600,
        log_level: "warn".to_string(),
        constraints_idle_timeout_sec: 1800,
        parse_timeout_ms: 5000,
    }
}

fn make_entry(id: &str, root: PathBuf) -> WorkspaceEntry {
    WorkspaceEntry {
        id: id.to_string(),
        root,
        languages: vec!["rust".to_string()],
        frozen: false,
    }
}

#[test]
fn test_symbol_kind_round_trip() {
    let kinds = [
        SymbolKind::Function,
        SymbolKind::Method,
        SymbolKind::Struct,
        SymbolKind::Enum,
        SymbolKind::Trait,
        SymbolKind::Impl,
        SymbolKind::Module,
        SymbolKind::Const,
        SymbolKind::Static,
        SymbolKind::TypeAlias,
        SymbolKind::Macro,
        SymbolKind::Class,
        SymbolKind::Mixin,
        SymbolKind::Extension,
    ];
    for kind in kinds {
        assert_eq!(kind.as_str().parse::<SymbolKind>(), Ok(kind));
    }
}

#[test]
fn test_symbol_kind_unknown() {
    assert!("bogus".parse::<SymbolKind>().is_err());
}

#[test]
fn test_ref_context_kind_round_trip() {
    let kinds = [
        RefContextKind::Call,
        RefContextKind::TypeUse,
        RefContextKind::Import,
        RefContextKind::FieldAccess,
        RefContextKind::PatternBind,
        RefContextKind::Other,
    ];
    for kind in kinds {
        assert_eq!(kind.as_str().parse::<RefContextKind>(), Ok(kind));
    }
}

#[test]
fn test_ref_context_kind_unknown() {
    assert!("bogus".parse::<RefContextKind>().is_err());
}

#[tokio::test]
async fn test_parse_fixture_directory() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(
        src.join("lib.rs"),
        "pub struct Config { pub name: String }\npub fn create_config() -> Config { Config { name: String::new() } }\n",
    )
    .unwrap();
    std::fs::write(
        src.join("main.rs"),
        "use crate::Config;\nfn main() { let _c = create_config(); }\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("fixture", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    let snap = {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    assert_eq!(snap.files_parsed, 2);
    assert!(
        snap.symbols_extracted >= 3,
        "expected at least Config, create_config, main"
    );
    assert!(snap.refs_extracted >= 1);
}

#[tokio::test]
async fn test_parse_empty_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("empty-pipeline", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    let snap = {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    assert_eq!(snap.files_parsed, 0);
    assert_eq!(snap.symbols_extracted, 0);
}

#[tokio::test]
async fn test_parse_skips_target_dir() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let target = dir.path().join("target").join("debug");
    std::fs::create_dir_all(&target).unwrap();

    std::fs::write(src.join("lib.rs"), "pub fn kept() {}\n").unwrap();
    std::fs::write(target.join("generated.rs"), "pub fn skipped() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("skip-target", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    let snap = {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    assert_eq!(snap.files_parsed, 1);
}

#[tokio::test]
async fn test_parse_rollups_populated() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(src.join("lib.rs"), "pub fn shared() {}\n").unwrap();
    std::fs::write(
        src.join("main.rs"),
        "use crate::shared;\nfn main() { shared(); }\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("rollups", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    let files = db.all_files().unwrap();
    let total_fan_in: i64 = files.iter().map(|f| f.fan_in_files).sum();
    let total_blast: i64 = files.iter().map(|f| f.blast_radius).sum();
    assert!(
        total_fan_in > 0 || total_blast > 0,
        "expected non-zero rollup values after cross-file parse"
    );
}

#[tokio::test]
async fn test_incremental_reparse() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    let file_path = src.join("lib.rs");
    std::fs::write(&file_path, "pub fn alpha() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("incremental-pipeline", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    let hash_before = db.file_by_path("src/lib.rs").unwrap().unwrap().content_hash;

    std::fs::write(&file_path, "pub fn alpha() {}\npub fn beta() {}\n").unwrap();
    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    let hash_after = db.file_by_path("src/lib.rs").unwrap().unwrap().content_hash;

    assert_ne!(hash_before, hash_after);
}

#[tokio::test]
async fn test_parse_snapshot_stored() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn hello() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("snapshot-stored", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    assert!(db.last_parse_time().unwrap().is_some());
}

#[tokio::test]
async fn test_unchanged_parse_copies_previous_snapshot_metrics() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn hello() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("unchanged-fast-snapshot", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    db.insert_snapshot(&SnapshotParams {
        total_complexity: 12_345,
        dead_symbol_count: 234,
        hotspot_count: 56,
        health_score: 7.25,
        pattern_family_count: 8,
        ..SnapshotParams::default()
    })
    .unwrap();

    let snap = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    assert_eq!(snap.files_parsed, 0);

    let latest = db.latest_snapshots(1).unwrap().pop().unwrap();
    assert_eq!(latest.files_parsed, 0);
    assert_eq!(latest.total_complexity, 12_345);
    assert_eq!(latest.dead_symbol_count, 234);
    assert_eq!(latest.hotspot_count, 56);
    assert_eq!(latest.health_score, 7.25);
    assert_eq!(latest.pattern_family_count, 8);
}

#[tokio::test]
async fn test_parent_symbol_id_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(
        src.join("lib.rs"),
        "pub struct Foo;\nimpl Foo {\n    pub fn bar(&self) {}\n    pub fn baz(&self) {}\n}\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("parent-id-rt", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    let file = db.file_by_path("src/lib.rs").unwrap().unwrap();
    let symbols = db.find_symbols_by_file(file.id).unwrap();

    let impl_sym = symbols
        .iter()
        .find(|s| &*s.kind == "impl")
        .expect("should have impl");
    let struct_sym = symbols
        .iter()
        .find(|s| &*s.kind == "struct")
        .expect("should have struct");

    // Struct is top-level — no parent
    assert!(
        struct_sym.parent_symbol_id.is_none(),
        "struct should have no parent"
    );
    // Impl is top-level — no parent
    assert!(
        impl_sym.parent_symbol_id.is_none(),
        "impl should have no parent"
    );

    // Methods should have impl as parent
    let methods: Vec<_> = symbols.iter().filter(|s| &*s.kind == "method").collect();
    assert_eq!(methods.len(), 2);
    for m in &methods {
        assert_eq!(
            m.parent_symbol_id,
            Some(impl_sym.id),
            "method {} should have impl as parent",
            m.short_name
        );
    }
}

#[tokio::test]
async fn test_nested_parent_symbol_id_chain() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(
        src.join("lib.rs"),
        "mod inner {\n    pub struct Foo;\n    impl Foo {\n        pub fn deep(&self) {}\n    }\n}\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("nested-parent", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    let file = db.file_by_path("src/lib.rs").unwrap().unwrap();
    let symbols = db.find_symbols_by_file(file.id).unwrap();

    let module = symbols
        .iter()
        .find(|s| &*s.kind == "module")
        .expect("should have module");
    let impl_sym = symbols
        .iter()
        .find(|s| &*s.kind == "impl")
        .expect("should have impl");
    let method = symbols
        .iter()
        .find(|s| &*s.kind == "method")
        .expect("should have method");

    // module is top-level
    assert!(module.parent_symbol_id.is_none());
    // impl is under module
    assert_eq!(impl_sym.parent_symbol_id, Some(module.id));
    // method is under impl
    assert_eq!(method.parent_symbol_id, Some(impl_sym.id));
}

#[test]
fn test_language_attrs_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
async fn fetch() -> Result<(), Error> {}
unsafe fn danger() {}
fn plain() {}
struct Borrowed<'a> { data: &'a str }
"#,
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: "test".into(),
        root: dir.path().to_path_buf(),
        languages: vec!["rust".into()],
        frozen: false,
    };
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked("test", db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();
    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let file = db.file_by_path("src/lib.rs").unwrap().unwrap();
    let symbols = db.find_symbols_by_file(file.id).unwrap();

    let fetch = symbols
        .iter()
        .find(|s| &*s.short_name == "fetch")
        .expect("fetch");
    let attrs: serde_json::Value = serde_json::from_str(
        fetch
            .language_attrs
            .as_deref()
            .expect("fetch should have attrs"),
    )
    .unwrap();
    assert_eq!(attrs["is_async"], true);
    assert_eq!(attrs["returns_result"], true);

    let danger = symbols
        .iter()
        .find(|s| &*s.short_name == "danger")
        .expect("danger");
    let attrs: serde_json::Value = serde_json::from_str(
        danger
            .language_attrs
            .as_deref()
            .expect("danger should have attrs"),
    )
    .unwrap();
    assert_eq!(attrs["is_unsafe"], true);

    let plain = symbols
        .iter()
        .find(|s| &*s.short_name == "plain")
        .expect("plain");
    assert_eq!(
        plain.language_attrs.as_deref(),
        Some("{}"),
        "plain fn should have empty attrs"
    );

    let borrowed = symbols
        .iter()
        .find(|s| &*s.short_name == "Borrowed")
        .expect("Borrowed");
    let attrs: serde_json::Value = serde_json::from_str(
        borrowed
            .language_attrs
            .as_deref()
            .expect("Borrowed should have attrs"),
    )
    .unwrap();
    assert_eq!(attrs["has_lifetime_params"], true);
}

#[test]
fn test_reparse_no_duplicate_symbols() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("lib.rs");
    std::fs::write(
        &src,
        "pub fn alpha() {}\npub fn beta() {}\nstruct Gamma { x: i32 }\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let config = make_config(db_dir.path());
    let entry = make_entry("reparse-test", dir.path().to_path_buf());
    let db = sutra::db::Db::open_unchecked(&entry.id, db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();

    // First parse.
    let snap1 = pipeline::parse_workspace(&entry, &db, &config, &cancel, &registry).unwrap();
    assert!(snap1.symbols_extracted > 0);

    // Modify the file to force a re-parse (different hash).
    std::fs::write(
        &src,
        "pub fn alpha() { 1 }\npub fn beta() {}\nstruct Gamma { x: i32 }\n",
    )
    .unwrap();

    // Second parse.
    let snap2 = pipeline::parse_workspace(&entry, &db, &config, &cancel, &registry).unwrap();
    assert!(snap2.symbols_extracted > 0);

    // Verify no duplicates: each (qualified_name, start_line) pair should appear once.
    let file = db
        .file_by_path("lib.rs")
        .unwrap()
        .expect("file should exist");
    let syms = db.find_symbols_by_file(file.id).unwrap();
    let mut seen = std::collections::HashSet::new();
    for s in &syms {
        let key = (Arc::clone(&s.qualified_name), s.start_line);
        assert!(
            seen.insert(key.clone()),
            "duplicate symbol: {} at line {}",
            key.0,
            key.1
        );
    }
}

/// End-to-end dead-symbol coverage for every Dart private-declaration form,
/// run through the real parse→resolve→derived pipeline (sutra/302).
///
/// Unit tests assert which *ref kinds* the parser emits; this asserts the
/// observable `sutra_dead` contract: a genuinely-unused private symbol of each
/// declaration form is reported dead, while a used one is not. It guards both
/// directions of the sutra/288 → sutra/302 history — the false positives
/// sutra/288 fixed (used private symbols wrongly dead) and the false negative
/// sutra/302 fixed (the `external static var _x;` / `identifier_list` form
/// self-referencing and masking a truly-dead symbol).
#[tokio::test]
async fn test_dart_dead_symbols_across_declaration_forms() {
    let dir = tempfile::tempdir().unwrap();
    let lib = dir.path().join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        lib.join("app.dart"),
        r#"
class _State {
  void _openChart() {}
  void _neverCalled() {}
  void build() {
    onPressed(_openChart);
  }
}

const _monthLengths = [31, 28, 31];
const _unusedConst = 7;

class Registry {
  external static var _liveStatic;
  external static var _deadStatic;
  int use() {
    return _liveStatic + _monthLengths[0];
  }
}

class _UnusedClass {}
class _UsedClass {}
_UsedClass make() {
  return _UsedClass();
}
"#,
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = WorkspaceEntry {
        id: "dart-dead".to_string(),
        root: dir.path().to_path_buf(),
        languages: vec!["dart".to_string()],
        frozen: false,
    };
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    }

    // Short names of everything reported dead (private symbols included).
    let dead: std::collections::HashSet<String> = db
        .find_dead_symbols(false, None)
        .unwrap()
        .into_iter()
        .map(|(qn, _path, _kind, _line, _vis)| qn.rsplit("::").next().unwrap_or(&qn).to_string())
        .collect();

    // Genuinely-unused private symbols must be reported dead, one per form:
    for expected in [
        "_neverCalled", // method
        "_unusedConst", // const
        "_deadStatic",  // external static var — the identifier_list form (sutra/302)
        "_UnusedClass", // class
    ] {
        assert!(
            dead.contains(expected),
            "expected `{expected}` to be reported dead; dead set = {dead:?}"
        );
    }

    // Used private symbols must NOT be dead — their intra-file references
    // (tear-off, const read, static read, construction) resolve (sutra/288):
    for live in ["_openChart", "_monthLengths", "_liveStatic", "_UsedClass"] {
        assert!(
            !dead.contains(live),
            "`{live}` is referenced and must not be reported dead; dead set = {dead:?}"
        );
    }
}
