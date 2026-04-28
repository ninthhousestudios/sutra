use std::path::PathBuf;

use sutra::config::Config;
use sutra::db::Db;
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
    assert!(snap.symbols_extracted >= 3, "expected at least Config, create_config, main");
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

    std::fs::write(
        src.join("lib.rs"),
        "pub fn shared() {}\n",
    )
    .unwrap();
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
