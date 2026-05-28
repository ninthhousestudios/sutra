use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use sutra::config::Config;
use sutra::db::Db;
use sutra::parser::{RefContextKind, SymbolKind};
use sutra::parser::adapter::default_registry;
use sutra::pipeline::{self, parse_changed_files};
use sutra::workspace::WorkspaceEntry;

fn make_config(db_dir: &std::path::Path) -> Config {
    Config {
        db_dir: db_dir.to_path_buf(),
        workspaces_path: db_dir.join("workspaces.toml"),
        listen_addr: "127.0.0.1:0".to_string(),
        parse_parallelism: 1,
        stale_threshold_sec: 600,
        log_level: "warn".to_string(),
        dd_idle_timeout_sec: 1800,
        parse_timeout_ms: 5000,
    }
}

fn make_entry(id: &str, root: PathBuf) -> WorkspaceEntry {
    WorkspaceEntry {
        id: id.to_string(),
        root,
        languages: vec!["rust".to_string()],
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

// --- incremental parse_changed_files tests ---

#[tokio::test]
async fn test_changed_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(src.join("lib.rs"), "pub fn alpha() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("changed-single", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    // Full parse first
    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    let hash_before = db.file_by_path("src/lib.rs").unwrap().unwrap().content_hash;

    // Modify the file on disk
    std::fs::write(src.join("lib.rs"), "pub fn alpha() {}\npub fn beta() {}\n").unwrap();

    // Incremental parse with only the changed file
    let cancel = AtomicBool::new(false);
    let registry = default_registry();
    let snap = parse_changed_files(&ws, &db, &config, &[src.join("lib.rs")], &[], &cancel, &registry).unwrap();

    assert_eq!(snap.files_parsed, 1);
    assert!(snap.symbols_extracted >= 2);
    let hash_after = db.file_by_path("src/lib.rs").unwrap().unwrap().content_hash;
    assert_ne!(hash_before, hash_after);
}

#[tokio::test]
async fn test_deleted_file_with_cascade() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(src.join("lib.rs"), "pub fn shared_fn() {}\n").unwrap();
    std::fs::write(
        src.join("main.rs"),
        "use crate::shared_fn;\nfn main() { shared_fn(); }\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("delete-cascade", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    // Full parse
    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    assert!(db.file_by_path("src/lib.rs").unwrap().is_some());

    // Delete lib.rs from disk
    std::fs::remove_file(src.join("lib.rs")).unwrap();

    // Incremental parse: lib.rs deleted
    let cancel = AtomicBool::new(false);
    let registry = default_registry();
    let snap = parse_changed_files(&ws, &db, &config, &[], &[src.join("lib.rs")], &cancel, &registry).unwrap();

    // lib.rs should be gone from DB
    assert!(db.file_by_path("src/lib.rs").unwrap().is_none());

    // main.rs should have been re-resolved (was in dirty set due to cascade)
    // The refs in main.rs that pointed to shared_fn should now be unresolved
    let main_file = db.file_by_path("src/main.rs").unwrap().unwrap();
    let refs = db.find_refs_in_file(main_file.id).unwrap();
    let resolved_to_shared: Vec<_> = refs
        .iter()
        .filter(|r| {
            r.unresolved_name.as_deref() == Some("shared_fn") && r.target_symbol_id.is_some()
        })
        .collect();
    assert!(
        resolved_to_shared.is_empty(),
        "refs to deleted symbol should be unresolved after cascade"
    );

    // Snapshot should reflect deletion work
    assert_eq!(snap.files_parsed, 0, "no files were parsed, only deleted");
}

#[tokio::test]
async fn test_multiple_files_changed() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(src.join("lib.rs"), "pub fn util() {}\n").unwrap();
    std::fs::write(src.join("main.rs"), "fn main() { util(); }\n").unwrap();
    std::fs::write(src.join("extra.rs"), "fn extra() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("multi-change", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    // Full parse
    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    // Modify two files, delete one
    std::fs::write(src.join("lib.rs"), "pub fn util() {}\npub fn util2() {}\n").unwrap();
    std::fs::write(src.join("main.rs"), "fn main() { util(); util2(); }\n").unwrap();
    std::fs::remove_file(src.join("extra.rs")).unwrap();

    let cancel = AtomicBool::new(false);
    let registry = default_registry();
    let snap = parse_changed_files(
        &ws,
        &db,
        &config,
        &[src.join("lib.rs"), src.join("main.rs")],
        &[src.join("extra.rs")],
        &cancel,
        &registry,
    )
    .unwrap();

    assert_eq!(snap.files_parsed, 2);
    assert!(db.file_by_path("src/extra.rs").unwrap().is_none());
    assert!(db.file_by_path("src/lib.rs").unwrap().is_some());
    assert!(db.file_by_path("src/main.rs").unwrap().is_some());

    // Rollups should be populated
    let files = db.all_files().unwrap();
    assert_eq!(files.len(), 2, "only lib.rs and main.rs remain");
}

#[tokio::test]
async fn test_parse_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..5 {
        std::fs::write(
            src.join(format!("mod{i}.rs")),
            format!("pub fn func_{i}() {{}}\n"),
        )
        .unwrap();
    }

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("cancel-test", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    let cancel = AtomicBool::new(true);
    let registry = default_registry();
    let result = pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry);
    assert!(result.is_err(), "pre-cancelled parse should fail");
}

#[tokio::test]
async fn test_incremental_skips_unlisted_language() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(src.join("lib.rs"), "pub fn hello() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("lang-filter", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    let cancel = AtomicBool::new(false);
    let registry = default_registry();
    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    // Add a .dart file — workspace only lists "rust"
    std::fs::write(src.join("app.dart"), "void main() {}\n").unwrap();

    let snap = parse_changed_files(
        &ws, &db, &config,
        &[src.join("app.dart")], &[],
        &cancel, &registry,
    ).unwrap();

    assert_eq!(snap.files_parsed, 0, "dart file should be skipped in rust-only workspace");
    assert!(db.file_by_path("src/app.dart").unwrap().is_none());
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

    let impl_sym = symbols.iter().find(|s| s.kind == "impl").expect("should have impl");
    let struct_sym = symbols.iter().find(|s| s.kind == "struct").expect("should have struct");

    // Struct is top-level — no parent
    assert!(struct_sym.parent_symbol_id.is_none(), "struct should have no parent");
    // Impl is top-level — no parent
    assert!(impl_sym.parent_symbol_id.is_none(), "impl should have no parent");

    // Methods should have impl as parent
    let methods: Vec<_> = symbols.iter().filter(|s| s.kind == "method").collect();
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

    let module = symbols.iter().find(|s| s.kind == "module").expect("should have module");
    let impl_sym = symbols.iter().find(|s| s.kind == "impl").expect("should have impl");
    let method = symbols.iter().find(|s| s.kind == "method").expect("should have method");

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
    let ws = WorkspaceEntry { id: "test".into(), root: dir.path().to_path_buf(), languages: vec!["rust".into()] };
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked("test", db_dir.path()).unwrap();
    let cancel = AtomicBool::new(false);
    let registry = default_registry();
    pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();

    let file = db.file_by_path("src/lib.rs").unwrap().unwrap();
    let symbols = db.find_symbols_by_file(file.id).unwrap();

    let fetch = symbols.iter().find(|s| s.short_name == "fetch").expect("fetch");
    let attrs: serde_json::Value =
        serde_json::from_str(fetch.language_attrs.as_deref().expect("fetch should have attrs"))
            .unwrap();
    assert_eq!(attrs["is_async"], true);
    assert_eq!(attrs["returns_result"], true);

    let danger = symbols.iter().find(|s| s.short_name == "danger").expect("danger");
    let attrs: serde_json::Value =
        serde_json::from_str(danger.language_attrs.as_deref().expect("danger should have attrs"))
            .unwrap();
    assert_eq!(attrs["is_unsafe"], true);

    let plain = symbols.iter().find(|s| s.short_name == "plain").expect("plain");
    assert_eq!(plain.language_attrs.as_deref(), Some("{}"), "plain fn should have empty attrs");

    let borrowed = symbols.iter().find(|s| s.short_name == "Borrowed").expect("Borrowed");
    let attrs: serde_json::Value =
        serde_json::from_str(borrowed.language_attrs.as_deref().expect("Borrowed should have attrs"))
            .unwrap();
    assert_eq!(attrs["has_lifetime_params"], true);
}
