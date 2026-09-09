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
    let outline_result =
        outline::handle(&db, "src/lib.rs", outline::OutlineDetail::Minimal).unwrap();
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

    // Freshly parsed: no byte drift, so the workspace is not stale — regardless
    // of elapsed time (there is no grace window).
    let (_, is_stale) = sutra::freshness::is_workspace_stale(&db, &ws.root, &ws.languages);
    assert!(!is_stale, "just-parsed workspace should not be stale");

    // Edit a source file: the content changes, so it must read as stale.
    std::fs::write(src.join("lib.rs"), "pub fn hello() { /* changed */ }\n").unwrap();
    let (_, is_stale) = sutra::freshness::is_workspace_stale(&db, &ws.root, &ws.languages);
    assert!(is_stale, "edited workspace should be stale");
}

/// Compute drift against the current index and incrementally reparse only that
/// set — the query-path refresh routine (sutra/363), driven directly.
fn refresh(ws: &WorkspaceEntry, db: &Db, config: &Config) -> pipeline::ParseSnapshot {
    let (_, drift) = sutra::freshness::workspace_drift(db, &ws.root, &ws.languages);
    let drift = drift.expect("baseline must exist for an incremental refresh");
    let registry = default_registry();
    pipeline::parse_incremental(ws, db, config, &registry, &drift).unwrap()
}

/// An edit to an indexed file is picked up by an incremental refresh that walks
/// only the drift set, and the new symbol is immediately queryable.
#[tokio::test]
async fn test_incremental_refresh_reflects_edit() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let file_path = src.join("lib.rs");
    std::fs::write(&file_path, "pub fn hello() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("inc_edit", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    }

    // The new symbol does not exist yet.
    let before = find::handle(&db, "added_later", None, None, false).unwrap();
    assert!(before["matches"].as_array().unwrap().is_empty());

    // Edit the file, adding a symbol, then refresh.
    std::fs::write(&file_path, "pub fn hello() {}\npub fn added_later() {}\n").unwrap();
    let snap = refresh(&ws, &db, &config);

    // Only the one drifted file was touched — no full workspace walk.
    assert_eq!(
        snap.files_walked, 1,
        "refresh must touch only the drift set"
    );
    assert_eq!(snap.files_parsed, 1);

    let after = find::handle(&db, "added_later", None, None, false).unwrap();
    assert!(
        !after["matches"].as_array().unwrap().is_empty(),
        "the incremental refresh must make the new symbol queryable"
    );

    // The refresh cleaned the drift — the next probe reads clean.
    let (_, is_stale) = sutra::freshness::is_workspace_stale(&db, &ws.root, &ws.languages);
    assert!(!is_stale, "workspace must be clean after the refresh");
}

/// A refresh handles a newly created file (added) and a deleted file (removed)
/// in one pass.
#[tokio::test]
async fn test_incremental_refresh_add_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.rs"), "pub fn alpha() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("inc_addrm", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    }
    assert!(db.file_by_path("src/a.rs").unwrap().is_some());

    // Add a new file and delete the original.
    std::fs::write(src.join("b.rs"), "pub fn beta() {}\n").unwrap();
    std::fs::remove_file(src.join("a.rs")).unwrap();

    refresh(&ws, &db, &config);

    assert!(
        db.file_by_path("src/a.rs").unwrap().is_none(),
        "removed file must be dropped from the index"
    );
    assert!(
        db.file_by_path("src/b.rs").unwrap().is_some(),
        "added file must be indexed"
    );
    let beta = find::handle(&db, "beta", None, None, false).unwrap();
    assert!(!beta["matches"].as_array().unwrap().is_empty());
}

/// The re-probe collapse that makes N racing queries produce ONE parse: once a
/// refresh has cleaned the drift, a subsequent refresh against the (now empty)
/// drift set is a no-op. The server re-probes under the parse lock and relies on
/// exactly this to skip redundant parses.
#[tokio::test]
async fn test_incremental_refresh_noop_when_clean() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("lib.rs"), "pub fn hello() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("inc_noop", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    }

    std::fs::write(src.join("lib.rs"), "pub fn hello() {}\npub fn more() {}\n").unwrap();
    let first = refresh(&ws, &db, &config);
    assert_eq!(first.files_parsed, 1, "first refresh reparses the edit");

    // Drift is now empty; a second refresh parses nothing.
    let second = refresh(&ws, &db, &config);
    assert_eq!(
        second.files_parsed, 0,
        "a clean workspace must not be reparsed"
    );
    assert_eq!(second.files_walked, 0);
}

/// Count refs in `caller_file` that resolve to symbol `sym` defined in
/// `def_file`.
fn inbound_resolved(db: &Db, def_file: &str, sym: &str, caller_file: &str) -> usize {
    let def_fid = db.file_by_path(def_file).unwrap().unwrap().id;
    let sym_id = db
        .find_symbols_by_file(def_fid)
        .unwrap()
        .into_iter()
        .find(|s| &*s.short_name == sym)
        .unwrap_or_else(|| panic!("symbol {sym} not found in {def_file}"))
        .id;
    let caller_fid = db.file_by_path(caller_file).unwrap().unwrap().id;
    db.find_refs_to_symbol(sym_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.file_id == caller_fid)
        .count()
}

/// sutra/378: an incremental reparse of a file must not silently drop resolved
/// INBOUND references from unchanged caller files. Reparsing the definition file
/// deletes and re-inserts its symbols (new ids); the caller's resolved ref rows
/// have no call-site name left (cleared on resolution) and would be cascade-
/// deleted with no way to reconstruct them. The definition file's replace path
/// detaches those inbound refs (name recovered from the target symbol) so
/// post-parse resolution re-links them to the new symbols.
#[tokio::test]
async fn test_incremental_refresh_preserves_inbound_refs() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.rs"), "pub fn target_fn() {}\n").unwrap();
    std::fs::write(src.join("b.rs"), "pub fn caller() { target_fn(); }\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("inc_inbound", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    }

    // Baseline: b.rs's call resolves to a.rs's target_fn.
    assert_eq!(
        inbound_resolved(&db, "src/a.rs", "target_fn", "src/b.rs"),
        1,
        "baseline: caller must resolve to target_fn"
    );

    // Edit a.rs (add a sibling symbol; target_fn itself is unchanged) and refresh
    // ONLY a.rs. b.rs is not re-extracted.
    std::fs::write(
        src.join("a.rs"),
        "pub fn target_fn() {}\npub fn sibling() {}\n",
    )
    .unwrap();
    let snap = refresh(&ws, &db, &config);
    assert_eq!(snap.files_parsed, 1, "only a.rs is reparsed");

    // The inbound edge from the untouched caller must survive the reparse.
    assert_eq!(
        inbound_resolved(&db, "src/a.rs", "target_fn", "src/b.rs"),
        1,
        "inbound ref from unchanged b.rs must survive a.rs's incremental reparse"
    );
}

/// sutra/378 variant: when the reparse removes the referenced symbol (rename),
/// the inbound ref must become correctly UNRESOLVED — its row preserved with the
/// call-site name — never silently dropped and never left pointing at a stale id.
#[tokio::test]
async fn test_incremental_refresh_inbound_ref_unresolves_on_rename() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.rs"), "pub fn target_fn() {}\n").unwrap();
    std::fs::write(src.join("b.rs"), "pub fn caller() { target_fn(); }\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("inc_inbound_rename", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    }
    assert_eq!(
        inbound_resolved(&db, "src/a.rs", "target_fn", "src/b.rs"),
        1
    );

    // Rename the referenced symbol out from under the caller.
    std::fs::write(src.join("a.rs"), "pub fn renamed_fn() {}\n").unwrap();
    refresh(&ws, &db, &config);

    // The caller's ref row is preserved but unresolved — not silently dropped.
    let caller_fid = db.file_by_path("src/b.rs").unwrap().unwrap().id;
    let refs = db.find_refs_in_file(caller_fid).unwrap();
    let call = refs
        .iter()
        .find(|r| r.context_kind == "call")
        .expect("caller's call ref must still exist, not be dropped");
    assert!(
        call.target_symbol_id.is_none(),
        "ref must be unresolved after the target was renamed away"
    );
    assert_eq!(
        call.unresolved_name.as_deref(),
        Some("target_fn"),
        "the call-site name must be preserved on the now-unresolved ref"
    );
}

/// Not a correctness test — measures query-path refresh latency for a one-file
/// edit on the sutra repo itself (sutra/363 acceptance criterion 3). Run:
/// `cargo test --test workspace_lifecycle_test measure_incremental_refresh -- --ignored --nocapture`.
#[tokio::test]
#[ignore]
async fn measure_incremental_refresh_on_self() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("self_bench", root.clone());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    // Setup (untimed): a full parse to build the baseline index.
    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    }

    // Simulate a one-file edit non-destructively: reparse one already-indexed
    // file (identical bytes) plus the full resolution tier.
    let one = db
        .file_by_path("src/pipeline.rs")
        .unwrap()
        .expect("src/pipeline.rs must be indexed");
    let drift = sutra::freshness::WorkspaceDrift {
        changed: vec![one.path.to_string()],
        added: vec![],
        removed: vec![],
    };
    let registry = default_registry();

    let start = std::time::Instant::now();
    let snap = pipeline::parse_incremental(&ws, &db, &config, &registry, &drift).unwrap();
    let elapsed = start.elapsed();
    println!(
        "one-file incremental refresh on sutra: {elapsed:?} (files_parsed={}, resolved={})",
        snap.files_parsed, snap.resolved_count
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "one-file refresh should complete well under 1s, took {elapsed:?}"
    );
}

/// A refresh over a file with broken syntax is never fatal: it returns Ok (the
/// server would answer normally with a note), keeps the rest of the index
/// intact, and reports the parse error.
#[tokio::test]
async fn test_incremental_refresh_bad_syntax_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("good.rs"), "pub fn good() {}\n").unwrap();
    std::fs::write(src.join("edit.rs"), "pub fn editable() {}\n").unwrap();

    let db_dir = tempfile::tempdir().unwrap();
    let ws = make_entry("inc_bad", dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let registry = default_registry();
        pipeline::parse_workspace(&ws, &db, &config, &cancel, &registry).unwrap();
    }

    // Corrupt one file's syntax.
    std::fs::write(src.join("edit.rs"), "pub fn editable( { { { unclosed\n").unwrap();

    let (_, drift) = sutra::freshness::workspace_drift(&db, &ws.root, &ws.languages);
    let drift = drift.unwrap();
    let registry = default_registry();
    // Must not return Err — a bad edit degrades, it does not break the query.
    let snap = pipeline::parse_incremental(&ws, &db, &config, &registry, &drift).unwrap();
    assert!(
        snap.parse_errors >= 1,
        "broken syntax should report a parse error"
    );

    // The untouched file's symbols survive.
    let good = find::handle(&db, "good", None, None, false).unwrap();
    assert!(!good["matches"].as_array().unwrap().is_empty());
}
