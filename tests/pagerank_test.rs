use std::path::PathBuf;

use sutra::config::Config;
use sutra::db::Db;
use sutra::parser::adapter::default_registry;
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
        dd_idle_timeout_sec: 1800,
    }
}

fn make_entry(id: &str, root: PathBuf) -> WorkspaceEntry {
    WorkspaceEntry {
        id: id.to_string(),
        root,
        languages: vec!["rust".to_string()],
    }
}

#[tokio::test]
async fn test_pagerank_populated_after_parse() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(
        src.join("lib.rs"),
        "pub fn shared() {}\npub fn other() {}\n",
    )
    .unwrap();
    std::fs::write(
        src.join("main.rs"),
        "use crate::shared;\nfn main() { shared(); }\n",
    )
    .unwrap();
    std::fs::write(
        src.join("util.rs"),
        "use crate::shared;\npub fn helper() { shared(); }\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("pr-test", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    let files = db.all_files().unwrap();
    let lib = files.iter().find(|f| f.path == "src/lib.rs").unwrap();
    assert!(lib.pagerank.is_some(), "lib.rs should have pagerank");
    assert!(
        lib.pagerank.unwrap() > 0.0,
        "lib.rs pagerank should be positive"
    );

    // lib.rs is depended on by main.rs and util.rs — should have highest PR.
    let main = files.iter().find(|f| f.path == "src/main.rs").unwrap();
    assert!(
        lib.pagerank.unwrap() > main.pagerank.unwrap_or(0.0),
        "lib.rs PR ({:?}) should exceed main.rs PR ({:?})",
        lib.pagerank,
        main.pagerank
    );
}

#[tokio::test]
async fn test_pagerank_sums_to_one() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(src.join("a.rs"), "pub fn fa() {}\n").unwrap();
    std::fs::write(src.join("b.rs"), "pub fn fb() {}\n").unwrap();
    std::fs::write(src.join("c.rs"), "pub fn fc() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("pr-sum", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    let files = db.all_files().unwrap();
    let sum: f64 = files.iter().filter_map(|f| f.pagerank).sum();
    assert!(
        (sum - 1.0).abs() < 0.01,
        "PageRank sum should be ~1.0, got {sum}"
    );
}

#[tokio::test]
async fn test_pagerank_warm_start_converges() {
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
    let ws = make_entry("pr-warm", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    // First parse — cold start.
    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    let files_first = db.all_files().unwrap();
    let _lib_pr1 = files_first
        .iter()
        .find(|f| f.path == "src/lib.rs")
        .unwrap()
        .pagerank
        .unwrap();

    // Second parse — warm start from existing ranks.
    // Add a new file to force reparse.
    std::fs::write(
        src.join("util.rs"),
        "use crate::shared;\npub fn helper() { shared(); }\n",
    )
    .unwrap();
    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();
    let files_second = db.all_files().unwrap();
    let lib_pr2 = files_second
        .iter()
        .find(|f| f.path == "src/lib.rs")
        .unwrap()
        .pagerank
        .unwrap();
    let main_pr2 = files_second
        .iter()
        .find(|f| f.path == "src/main.rs")
        .unwrap()
        .pagerank
        .unwrap_or(0.0);

    // Warm-started PR converges correctly — lib.rs still highest.
    assert!(
        lib_pr2 > 0.0,
        "lib.rs should have positive PR after warm start"
    );
    assert!(
        lib_pr2 > main_pr2,
        "lib.rs ({lib_pr2}) should still outrank main.rs ({main_pr2}) after warm start"
    );

    // Sum still ~1.0.
    let sum: f64 = files_second.iter().filter_map(|f| f.pagerank).sum();
    assert!(
        (sum - 1.0).abs() < 0.01,
        "PR sum should be ~1.0 after warm start, got {sum}"
    );
}

#[tokio::test]
async fn test_symbol_pagerank_distributed() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    std::fs::write(
        src.join("lib.rs"),
        "pub fn hot_fn() {}\npub fn cold_fn() {}\n",
    )
    .unwrap();
    std::fs::write(
        src.join("main.rs"),
        "use crate::hot_fn;\nfn main() { hot_fn(); hot_fn(); }\n",
    )
    .unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("sym-pr", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    let hot = db.resolve_symbol("hot_fn", None).unwrap();
    let cold = db.resolve_symbol("cold_fn", None).unwrap();

    if let (Some(h), Some(c)) = (hot, cold) {
        assert!(h.pagerank.is_some(), "hot_fn should have pagerank");
        assert!(c.pagerank.is_some(), "cold_fn should have pagerank");
        assert!(
            h.pagerank.unwrap() > c.pagerank.unwrap(),
            "hot_fn PR ({:?}) should exceed cold_fn PR ({:?})",
            h.pagerank,
            c.pagerank
        );
    }
}

#[tokio::test]
async fn test_dangling_node_handling() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();

    // isolated.rs has no imports and nothing imports it — dangling node.
    std::fs::write(src.join("isolated.rs"), "pub fn lonely() {}\n").unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn entry() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("dangling", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry)
    }
    .unwrap();

    let files = db.all_files().unwrap();
    for f in &files {
        assert!(
            f.pagerank.is_some() && f.pagerank.unwrap() > 0.0,
            "all files should have positive PageRank, {} has {:?}",
            f.path,
            f.pagerank
        );
    }
}
