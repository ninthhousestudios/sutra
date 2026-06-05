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

fn codebook_count(db: &Db) -> usize {
    let conn = db.conn_for_test();
    conn.query_row("SELECT COUNT(*) FROM hrr_codebook", [], |row| row.get::<_, i64>(0))
        .unwrap() as usize
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
    let f = setup(&[("src/lib.rs", "pub fn hello() -> i32 { 42 }\npub fn world() -> i32 { 0 }\n")]);
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

    // Reindex (drops ephemeral tables including hrr_vectors, but keeps hrr_codebook)
    f.db.reindex().unwrap();
    parse(&f);
    let vecs2 = load_vectors(&f.db);

    assert_eq!(vecs1.len(), vecs2.len());
    for (a, b) in vecs1.iter().zip(vecs2.iter()) {
        assert_eq!(a.1, b.1, "mode mismatch");
        assert_eq!(a.2.data, b.2.data, "vectors differ after reparse for mode={}", a.1);
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
fn codebook_persists_across_reindex() {
    let f = setup(&[("src/lib.rs", "pub fn persisted(x: i32) -> i32 { x }\n")]);
    parse(&f);

    let count_before = codebook_count(&f.db);
    assert!(count_before > 0, "codebook should have entries after parse");

    // Reindex drops ephemeral tables but keeps durable hrr_codebook
    f.db.reindex().unwrap();
    let count_after = codebook_count(&f.db);
    assert_eq!(count_before, count_after, "codebook should survive reindex");
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

    let result = sutra::tools::similar::handle(&f.db, "alpha", Some("strip"), Some(10), Some(0.0))
        .unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "should find at least one similar function");

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

    let strip_result =
        sutra::tools::similar::handle(&f.db, "alpha", Some("strip"), Some(10), Some(0.0)).unwrap();
    let embed_result =
        sutra::tools::similar::handle(&f.db, "alpha", Some("embed"), Some(10), Some(0.0)).unwrap();

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
    let f = setup(&[(
        "src/lib.rs",
        "pub fn only_one(x: i32) -> i32 { x + 1 }\n",
    )]);
    parse(&f);

    let result =
        sutra::tools::similar::handle(&f.db, "only_one", Some("strip"), Some(10), Some(0.0))
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

    let result =
        sutra::tools::similar::handle(&f.db, "does_not_exist", Some("strip"), None, None).unwrap();
    assert!(
        result.get("diagnostic").is_some(),
        "unknown symbol should return a diagnostic"
    );
}

#[test]
fn similar_search_non_function_symbol() {
    let f = setup(&[(
        "src/lib.rs",
        concat!("pub struct MyStruct { pub x: i32 }\n", "pub fn helper() {}\n"),
    )]);
    parse(&f);

    let result =
        sutra::tools::similar::handle(&f.db, "MyStruct", Some("strip"), None, None).unwrap();
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
    let result = sutra::tools::similar::handle(&f.db, "add", Some("strip"), Some(10), Some(0.0))
        .unwrap();
    let matches = result["matches"].as_array().unwrap();
    assert!(!matches.is_empty(), "should find matches for add");

    let top_sim = matches[0]["similarity"].as_f64().unwrap();
    assert!(
        top_sim < 0.99,
        "functions differing only by operator should not be near-identical in strip mode, sim={top_sim:.4}"
    );
}
