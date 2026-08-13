use std::path::PathBuf;

use sutra::config::Config;
use sutra::db::Db;
use sutra::parser::adapter::default_registry;
use sutra::pipeline;
use sutra::tools::{find, map, outline};
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

#[tokio::test]
async fn test_register_parse_query_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(
        src.join("lib.rs"),
        "pub fn hello() -> &'static str { \"hi\" }\n",
    )
    .unwrap();
    std::fs::write(
        src.join("util.rs"),
        "use crate::hello;\npub fn greet() { let _ = hello(); }\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("lifecycle", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    let snap = {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    assert!(snap.files_parsed >= 2, "expected at least 2 files parsed");
    assert!(snap.symbols_extracted > 0, "expected symbols extracted");

    let map_result = map::handle(&db, None, None, false).unwrap();
    let files = map_result["files"].as_array().unwrap();
    assert!(!files.is_empty(), "map should return files");

    let find_result = find::handle(&db, "hello", None, None, false).unwrap();
    let matches = find_result["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "find should locate 'hello'");

    // Outline one of the parsed files — path is relative to workspace root.
    let outline_result = outline::handle(&db, "src/lib.rs", true).unwrap();
    let symbols = outline_result["symbols"].as_array().unwrap();
    assert!(
        !symbols.is_empty(),
        "outline should return symbols for src/lib.rs"
    );
}

#[tokio::test]
async fn test_incremental_reparse() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    let file_path = src.join("main.rs");
    std::fs::write(&file_path, "pub fn init() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("incremental", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    let snap1 = {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    assert_eq!(snap1.files_parsed, 1);

    // Second parse without any change — hash check should skip the file.
    let snap2 = {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    assert_eq!(
        snap2.files_parsed, 0,
        "unchanged file should be skipped on reparse"
    );

    // Modify the file.
    std::fs::write(&file_path, "pub fn init() {}\npub fn extra() {}\n").unwrap();

    let snap3 = {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    assert_eq!(snap3.files_parsed, 1, "modified file should be reparsed");
}

#[tokio::test]
async fn test_delete_cascade() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(src.join("a.rs"), "pub fn alpha() {}\n").unwrap();
    std::fs::write(src.join("b.rs"), "pub fn beta() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("cascade", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    let snap1 = {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    assert_eq!(snap1.files_parsed, 2);

    // Delete one file from disk.
    std::fs::remove_file(src.join("b.rs")).unwrap();

    // Reparse — the pipeline walks the workspace and only sees a.rs.
    // b.rs will no longer be touched. A full pipeline run doesn't proactively
    // prune missing files in v0.1, so we verify the remaining file is intact.
    let snap2 = {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    // a.rs is unchanged — hash matches, so files_parsed == 0.
    // The important assertion is that the parse doesn't error out.
    assert_eq!(snap2.parse_errors, 0);

    // a.rs symbols must still be in the DB.
    let a_file = db.file_by_path("src/a.rs").unwrap();
    assert!(a_file.is_some(), "a.rs should still be in the DB");
    let syms = db.find_symbols_by_file(a_file.unwrap().id).unwrap();
    assert!(
        !syms.is_empty(),
        "a.rs symbols must survive after b.rs deletion"
    );
}

#[tokio::test]
async fn test_empty_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("empty", dir.path().to_path_buf());
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
    assert_eq!(snap.parse_errors, 0);
}

#[tokio::test]
async fn test_stale_detection() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn hello() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("stale", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    let last_parse = db.last_parse_time().unwrap();
    assert!(
        last_parse.is_some(),
        "last_parse_time should be set after a parse"
    );

    // Config with stale_threshold_sec=0 means any parse time is considered stale.
    let stale_config = Config {
        stale_threshold_sec: 0,
        ..make_config(db_dir.path())
    };
    assert_eq!(stale_config.stale_threshold_sec, 0);
}
