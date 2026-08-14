use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use sutra::config::Config;
use sutra::db::Db;
use sutra::parser::adapter::default_registry;
use sutra::similarity::hrr::HrrVec;
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

struct Fixture {
    _ws_dir: tempfile::TempDir,
    _db_dir: tempfile::TempDir,
    db: Db,
    ws: WorkspaceEntry,
    config: Config,
}

fn setup(files: &[(&str, &str)]) -> Fixture {
    let ws_dir = tempfile::tempdir().unwrap();
    let db_dir = tempfile::tempdir().unwrap();

    for (path, content) in files {
        let full = ws_dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(&full, content).unwrap();
    }

    let ws = make_entry("sim-test", ws_dir.path().to_path_buf());
    let config = make_config(db_dir.path());
    let db = Db::open_unchecked(&ws.id, db_dir.path()).unwrap();

    Fixture {
        _ws_dir: ws_dir,
        _db_dir: db_dir,
        db,
        ws,
        config,
    }
}

fn parse(f: &Fixture) {
    let cancel = AtomicBool::new(false);
    let registry = default_registry();
    sutra::pipeline::parse_workspace(&f.ws, &f.db, &f.config, &cancel, &registry).unwrap();
}

fn load_vectors(db: &Db) -> Vec<(i64, String, HrrVec)> {
    let conn = db.conn_for_test();
    let mut stmt = conn
        .prepare("SELECT symbol_id, mode, vector FROM hrr_vectors ORDER BY symbol_id, mode")
        .unwrap();
    stmt.query_map([], |row| {
        let sym_id: i64 = row.get(0)?;
        let mode: String = row.get(1)?;
        let blob: Vec<u8> = row.get(2)?;
        Ok((sym_id, mode, HrrVec::from_bytes(&blob)))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

fn get_vec<'a>(vecs: &'a [(i64, String, HrrVec)], sym_id: i64, mode: &str) -> &'a HrrVec {
    vecs.iter()
        .find(|(id, m, _)| *id == sym_id && m == mode)
        .map(|(_, _, v)| v)
        .unwrap_or_else(|| panic!("no vector for sym_id={sym_id} mode={mode}"))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn hrr_vectors_produced_for_functions() {
    let f = setup(&[(
        "src/lib.rs",
        "pub fn hello() -> i32 { 42 }\npub fn world() -> i32 { 0 }\n",
    )]);
    parse(&f);

    let vecs = load_vectors(&f.db);
    // 2 functions × 2 modes = 4 vectors
    assert_eq!(vecs.len(), 4, "expected 4 vectors, got {}", vecs.len());

    let symbols = f.db.function_symbols_for_hrr().unwrap();
    assert_eq!(symbols.len(), 2);

    for sym in &symbols {
        let strip = get_vec(&vecs, sym.symbol_id, "strip");
        let embed = get_vec(&vecs, sym.symbol_id, "embed");
        assert_eq!(strip.data.len(), 1024);
        assert_eq!(embed.data.len(), 1024);
    }
}

#[test]
fn determinism_same_tree_same_vector() {
    let source = "pub fn deterministic(x: i32, y: i32) -> i32 { x + y }\n";
    let f = setup(&[("src/lib.rs", source)]);
    parse(&f);
    let vecs1 = load_vectors(&f.db);

    // Reindex drops ephemeral tables including hrr_vectors; the codebook is
    // content-addressed (sutra/327), so re-encoding must reproduce the same
    // vectors from scratch.
    f.db.reindex().unwrap();
    parse(&f);
    let vecs2 = load_vectors(&f.db);

    assert_eq!(vecs1.len(), vecs2.len());
    for (a, b) in vecs1.iter().zip(vecs2.iter()) {
        assert_eq!(a.1, b.1, "mode mismatch");
        assert_eq!(
            a.2.data, b.2.data,
            "vectors differ after reparse for mode={}",
            a.1
        );
    }
}

#[test]
fn discrimination_different_trees() {
    let f = setup(&[(
        "src/lib.rs",
        concat!(
            "pub fn linear(x: i32) -> i32 { x + 1 }\n",
            "pub fn branching(x: i32) -> i32 { if x > 0 { x * 2 } else { x - 1 } }\n",
        ),
    )]);
    parse(&f);

    let symbols = f.db.function_symbols_for_hrr().unwrap();
    assert_eq!(symbols.len(), 2);
    let vecs = load_vectors(&f.db);

    let strip_a = get_vec(&vecs, symbols[0].symbol_id, "strip");
    let strip_b = get_vec(&vecs, symbols[1].symbol_id, "strip");
    let sim = strip_a.cosine_similarity(strip_b);
    assert!(
        sim < 0.9,
        "structurally different functions should have sim < 0.9, got {sim:.4}"
    );
}

#[test]
fn position_sensitivity_reordered_params() {
    let f1 = setup(&[("src/lib.rs", "pub fn foo(a: i32, b: String) -> i32 { 0 }\n")]);
    parse(&f1);
    let v1 = load_vectors(&f1.db);
    let strip1 = &v1.iter().find(|(_, m, _)| m == "strip").unwrap().2;

    let f2 = setup(&[("src/lib.rs", "pub fn foo(b: String, a: i32) -> i32 { 0 }\n")]);
    parse(&f2);
    let v2 = load_vectors(&f2.db);
    let strip2 = &v2.iter().find(|(_, m, _)| m == "strip").unwrap().2;

    let sim = strip1.cosine_similarity(strip2);
    assert!(
        sim < 0.99,
        "reordered params should produce different strip vectors, sim={sim:.4}"
    );
}

#[test]
fn strip_ignores_identifiers_embed_distinguishes() {
    // Single fixture so both functions share one codebook — different keys get
    // different random vectors, making the embed mode comparison meaningful.
    let f = setup(&[(
        "src/lib.rs",
        concat!(
            "pub fn variant_xy(x: i32, y: i32) -> i32 { x + y }\n",
            "pub fn variant_ab(a: i32, b: i32) -> i32 { a + b }\n",
        ),
    )]);
    parse(&f);

    let symbols = f.db.function_symbols_for_hrr().unwrap();
    assert_eq!(symbols.len(), 2);
    let vecs = load_vectors(&f.db);

    // Strip mode: identical tree structure, identifiers ignored → identical vectors
    let strip_1 = get_vec(&vecs, symbols[0].symbol_id, "strip");
    let strip_2 = get_vec(&vecs, symbols[1].symbol_id, "strip");
    let strip_sim = strip_1.cosine_similarity(strip_2);
    assert!(
        (strip_sim - 1.0).abs() < 1e-10,
        "strip vectors should be identical for same structure, sim={strip_sim:.4}"
    );

    // Embed mode: same structure but different identifier text → different vectors
    let embed_1 = get_vec(&vecs, symbols[0].symbol_id, "embed");
    let embed_2 = get_vec(&vecs, symbols[1].symbol_id, "embed");
    let embed_sim = embed_1.cosine_similarity(embed_2);
    assert!(
        embed_sim < 0.99,
        "embed vectors should differ when identifiers differ, sim={embed_sim:.4}"
    );
    assert!(
        embed_sim < strip_sim,
        "embed should be less similar than strip: embed={embed_sim:.4} strip={strip_sim:.4}"
    );
}

#[test]
fn methods_also_get_vectors() {
    let f = setup(&[(
        "src/lib.rs",
        concat!(
            "pub struct Foo;\n",
            "impl Foo {\n",
            "    pub fn bar(&self) -> i32 { 1 }\n",
            "    pub fn baz(&self, x: i32) -> i32 { x }\n",
            "}\n",
        ),
    )]);
    parse(&f);

    let symbols = f.db.function_symbols_for_hrr().unwrap();
    assert_eq!(symbols.len(), 2, "expected 2 methods");

    let vecs = load_vectors(&f.db);
    assert_eq!(vecs.len(), 4, "expected 4 vectors (2 methods × 2 modes)");
}

// ---------------------------------------------------------------------------
// sutra_similar tool tests
// ---------------------------------------------------------------------------

#[test]
fn similar_search_strip_mode() {
    let f = setup(&[(
        "src/lib.rs",
        concat!(
            "pub fn alpha(x: i32, y: i32) -> i32 { x + y }\n",
            "pub fn beta(a: i32, b: i32) -> i32 { a + b }\n",
            "pub fn gamma(x: i32) -> i32 { if x > 0 { x * 2 } else { x - 1 } }\n",
        ),
    )]);
    parse(&f);

    let result = sutra::tools::similar::handle(
        &f.db,
        Some("alpha"),
        Some("strip"),
        Some(10),
        Some(0.0),
        None,
    )
    .unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(
        !matches.is_empty(),
        "should find at least one similar function"
    );

    // beta has identical structure to alpha in strip mode — should be top match
    let top = &matches[0];
    assert_eq!(top["symbol"].as_str().unwrap(), "beta");
    let sim = top["similarity"].as_f64().unwrap();
    assert!(
        (sim - 1.0).abs() < 0.01,
        "identical structure should have sim ~1.0, got {sim}"
    );
}

#[test]
fn similar_search_embed_mode_lower_than_strip() {
    let f = setup(&[(
        "src/lib.rs",
        concat!(
            "pub fn alpha(x: i32, y: i32) -> i32 { x + y }\n",
            "pub fn beta(a: i32, b: i32) -> i32 { a + b }\n",
        ),
    )]);
    parse(&f);

    let strip_result = sutra::tools::similar::handle(
        &f.db,
        Some("alpha"),
        Some("strip"),
        Some(10),
        Some(0.0),
        None,
    )
    .unwrap();
    let embed_result = sutra::tools::similar::handle(
        &f.db,
        Some("alpha"),
        Some("embed"),
        Some(10),
        Some(0.0),
        None,
    )
    .unwrap();

    let strip_sim = strip_result["matches"][0]["similarity"].as_f64().unwrap();
    let embed_sim = embed_result["matches"][0]["similarity"].as_f64().unwrap();

    assert!(
        embed_sim < strip_sim,
        "embed similarity should be lower than strip when names differ: \
         embed={embed_sim:.4} strip={strip_sim:.4}"
    );
}

#[test]
fn similar_search_excludes_self() {
    let f = setup(&[("src/lib.rs", "pub fn only_one(x: i32) -> i32 { x + 1 }\n")]);
    parse(&f);

    let result = sutra::tools::similar::handle(
        &f.db,
        Some("only_one"),
        Some("strip"),
        Some(10),
        Some(0.0),
        None,
    )
    .unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(
        matches.is_empty(),
        "single function should have no matches (self excluded)"
    );
}

#[test]
fn similar_search_unknown_symbol() {
    let f = setup(&[("src/lib.rs", "pub fn exists() {}\n")]);
    parse(&f);

    let result = sutra::tools::similar::handle(
        &f.db,
        Some("does_not_exist"),
        Some("strip"),
        None,
        None,
        None,
    )
    .unwrap();
    assert!(
        result.get("diagnostic").is_some(),
        "unknown symbol should return a diagnostic"
    );
}

#[test]
fn similar_search_non_function_symbol() {
    let f = setup(&[(
        "src/lib.rs",
        concat!(
            "pub struct MyStruct { pub x: i32 }\n",
            "pub fn helper() {}\n"
        ),
    )]);
    parse(&f);

    let result =
        sutra::tools::similar::handle(&f.db, Some("MyStruct"), Some("strip"), None, None, None)
            .unwrap();
    assert!(
        result.get("diagnostic").is_some(),
        "struct symbol should return a diagnostic about function-only search"
    );
}

#[test]
fn operator_discrimination() {
    let f = setup(&[(
        "src/lib.rs",
        concat!(
            "pub fn add(x: i32, y: i32) -> i32 { x + y }\n",
            "pub fn sub(x: i32, y: i32) -> i32 { x - y }\n",
            "pub fn mul(x: i32, y: i32) -> i32 { x * y }\n",
        ),
    )]);
    parse(&f);

    let _vecs = load_vectors(&f.db);
    // Use sutra_similar to find matches for "add" — sub and mul should not be ~1.0
    let result =
        sutra::tools::similar::handle(&f.db, Some("add"), Some("strip"), Some(10), Some(0.0), None)
            .unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "should find matches for add");

    let top_sim = matches[0]["similarity"].as_f64().unwrap();
    assert!(
        top_sim < 0.99,
        "functions differing only by operator should not be near-identical in strip mode, sim={top_sim:.4}"
    );
}

// ---------------------------------------------------------------------------
// Incremental HRR recomputation (sutra/142)
// ---------------------------------------------------------------------------

#[test]
fn no_change_recompute_is_noop() {
    let f = setup(&[(
        "src/lib.rs",
        "pub fn hello() -> i32 { 42 }\npub fn world() -> i32 { 0 }\n",
    )]);
    parse(&f);

    let vecs_before = load_vectors(&f.db);
    assert_eq!(
        vecs_before.len(),
        4,
        "initial parse should produce 4 vectors"
    );

    let (count, changed) =
        sutra::similarity::compute_hrr_vectors(&f.db, f.ws.root.as_path()).unwrap();
    assert_eq!(count, 0, "no-change recompute should process zero symbols");
    assert!(!changed, "no-change recompute should report no change");

    let vecs_after = load_vectors(&f.db);
    assert_eq!(vecs_before.len(), vecs_after.len());
    for (before, after) in vecs_before.iter().zip(vecs_after.iter()) {
        assert_eq!(before.0, after.0, "symbol_id should be unchanged");
        assert_eq!(before.1, after.1, "mode should be unchanged");
        assert_eq!(
            before.2.data, after.2.data,
            "vector data should be identical"
        );
    }
}

#[test]
fn single_file_change_recomputes_only_that_file() {
    let f = setup(&[
        ("src/a.rs", "pub fn alpha() -> i32 { 1 }\n"),
        ("src/b.rs", "pub fn beta() -> i32 { 2 }\n"),
    ]);
    parse(&f);

    let vecs_after_first = load_vectors(&f.db);
    assert_eq!(
        vecs_after_first.len(),
        4,
        "2 functions × 2 modes = 4 vectors"
    );

    let syms = f.db.function_symbols_for_hrr().unwrap();
    let a_sym = syms.iter().find(|s| s.file_path == "src/a.rs").unwrap();
    let b_sym_id_before = syms
        .iter()
        .find(|s| s.file_path == "src/b.rs")
        .unwrap()
        .symbol_id;
    let b_strip_before = get_vec(&vecs_after_first, b_sym_id_before, "strip")
        .data
        .clone();
    let _a_strip_before = get_vec(&vecs_after_first, a_sym.symbol_id, "strip")
        .data
        .clone();

    // Modify only file a
    std::fs::write(
        f.ws.root.join("src/a.rs"),
        "pub fn alpha(x: i32) -> i32 { x + 1 }\n",
    )
    .unwrap();

    // Re-parse (full re-parse after modifying a.rs on disk)
    let cancel = AtomicBool::new(false);
    let registry = default_registry();
    sutra::pipeline::parse_workspace(&f.ws, &f.db, &f.config, &cancel, &registry).unwrap();

    // b's vectors should be unchanged (same symbol_id, same data)
    let vecs_after_change = load_vectors(&f.db);
    let syms_after = f.db.function_symbols_for_hrr().unwrap();
    let b_sym_after = syms_after
        .iter()
        .find(|s| s.file_path == "src/b.rs")
        .unwrap();
    assert_eq!(
        b_sym_after.symbol_id, b_sym_id_before,
        "unchanged file should keep its symbol_id"
    );
    let b_strip_after = get_vec(&vecs_after_change, b_sym_after.symbol_id, "strip");
    assert_eq!(
        b_strip_before, b_strip_after.data,
        "unchanged file's vectors should be identical"
    );

    // a's vectors should exist (with a new symbol_id since the file was re-parsed)
    let a_sym_after = syms_after
        .iter()
        .find(|s| s.file_path == "src/a.rs")
        .unwrap();
    assert!(
        get_vec(&vecs_after_change, a_sym_after.symbol_id, "strip")
            .data
            .len()
            == 1024,
        "changed file should have new vectors"
    );
}

#[test]
fn hrr_file_hashes_written_atomically_with_vectors() {
    let f = setup(&[("src/lib.rs", "pub fn hello() -> i32 { 42 }\n")]);
    parse(&f);

    let conn = f.db.conn_for_test();
    let hash_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM hrr_file_hashes", [], |r| r.get(0))
        .unwrap();
    assert!(hash_count > 0, "should have file hashes after parse");

    let mismatched: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM hrr_file_hashes h
             JOIN files f ON h.file_id = f.id
             WHERE h.content_hash != f.content_hash",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        mismatched, 0,
        "all hrr_file_hashes should match current files.content_hash"
    );
}
