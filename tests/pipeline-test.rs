use std::path::PathBuf;

use sutra::config::Config;
use sutra::db::Db;
use sutra::parser::{RefContextKind, SymbolKind};
use sutra::pipeline::{self, parse_changed_files};
use sutra::workspace::WorkspaceEntry;

fn make_config(db_dir: &std::path::Path) -> Config {
    Config {
        db_dir: db_dir.to_path_buf(),
        workspaces_path: db_dir.join("workspaces.toml"),
        listen_addr: "127.0.0.1:0".to_string(),
        parse_parallelism: 1,
        stale_threshold_sec: 600,
        watch_poll_sec: 2,
        watch_debounce_sec: 3,
        log_level: "warn".to_string(),
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
    let db = Db::open(&ws.id, db_dir.path()).unwrap();

    let snap = pipeline::parse_workspace(&ws, &db, &config).await.unwrap();
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
    let db = Db::open(&ws.id, db_dir.path()).unwrap();

    let snap = pipeline::parse_workspace(&ws, &db, &config).await.unwrap();
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
    let db = Db::open(&ws.id, db_dir.path()).unwrap();

    let snap = pipeline::parse_workspace(&ws, &db, &config).await.unwrap();
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
    let db = Db::open(&ws.id, db_dir.path()).unwrap();

    pipeline::parse_workspace(&ws, &db, &config).await.unwrap();

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
    let db = Db::open(&ws.id, db_dir.path()).unwrap();

    pipeline::parse_workspace(&ws, &db, &config).await.unwrap();
    let hash_before = db.file_by_path("src/lib.rs").unwrap().unwrap().content_hash;

    std::fs::write(&file_path, "pub fn alpha() {}\npub fn beta() {}\n").unwrap();
    pipeline::parse_workspace(&ws, &db, &config).await.unwrap();
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
    let db = Db::open(&ws.id, db_dir.path()).unwrap();

    pipeline::parse_workspace(&ws, &db, &config).await.unwrap();

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
    let db = Db::open(&ws.id, db_dir.path()).unwrap();

    // Full parse first
    pipeline::parse_workspace(&ws, &db, &config).await.unwrap();
    let hash_before = db.file_by_path("src/lib.rs").unwrap().unwrap().content_hash;

    // Modify the file on disk
    std::fs::write(src.join("lib.rs"), "pub fn alpha() {}\npub fn beta() {}\n").unwrap();

    // Incremental parse with only the changed file
    let snap = parse_changed_files(&ws, &db, &config, &[src.join("lib.rs")], &[])
        .await
        .unwrap();

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
    let db = Db::open(&ws.id, db_dir.path()).unwrap();

    // Full parse
    pipeline::parse_workspace(&ws, &db, &config).await.unwrap();
    assert!(db.file_by_path("src/lib.rs").unwrap().is_some());

    // Delete lib.rs from disk
    std::fs::remove_file(src.join("lib.rs")).unwrap();

    // Incremental parse: lib.rs deleted
    let snap = parse_changed_files(&ws, &db, &config, &[], &[src.join("lib.rs")])
        .await
        .unwrap();

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
    let db = Db::open(&ws.id, db_dir.path()).unwrap();

    // Full parse
    pipeline::parse_workspace(&ws, &db, &config).await.unwrap();

    // Modify two files, delete one
    std::fs::write(src.join("lib.rs"), "pub fn util() {}\npub fn util2() {}\n").unwrap();
    std::fs::write(src.join("main.rs"), "fn main() { util(); util2(); }\n").unwrap();
    std::fs::remove_file(src.join("extra.rs")).unwrap();

    let snap = parse_changed_files(
        &ws,
        &db,
        &config,
        &[src.join("lib.rs"), src.join("main.rs")],
        &[src.join("extra.rs")],
    )
    .await
    .unwrap();

    assert_eq!(snap.files_parsed, 2);
    assert!(db.file_by_path("src/extra.rs").unwrap().is_none());
    assert!(db.file_by_path("src/lib.rs").unwrap().is_some());
    assert!(db.file_by_path("src/main.rs").unwrap().is_some());

    // Rollups should be populated
    let files = db.all_files().unwrap();
    assert_eq!(files.len(), 2, "only lib.rs and main.rs remain");
}
