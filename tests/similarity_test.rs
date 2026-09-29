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

fn similar(
    f: &Fixture,
    symbol: &str,
    mode: Option<&str>,
    limit: Option<usize>,
    threshold: Option<f64>,
) -> serde_json::Value {
    let args = sutra::tools::similar::SimilarArgs {
        workspace: String::new(),
        symbol: symbol.to_string(),
        mode: mode.map(str::to_string),
        limit,
        threshold,
    };
    sutra::tools::similar::handle(&f.db, &f.ws.root, &default_registry(), &args).unwrap()
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

    let result = similar(&f, "alpha", Some("strip"), Some(10), Some(0.0));
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

    let strip_result = similar(&f, "alpha", Some("strip"), Some(10), Some(0.0));
    let embed_result = similar(&f, "alpha", Some("embed"), Some(10), Some(0.0));

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

    let result = similar(&f, "only_one", Some("strip"), Some(10), Some(0.0));
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

    let result = similar(&f, "does_not_exist", Some("strip"), None, None);
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

    let result = similar(&f, "MyStruct", Some("strip"), None, None);
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
    let result = similar(&f, "add", Some("strip"), Some(10), Some(0.0));
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

    let count = sutra::similarity::compute_hrr_vectors(&f.db, f.ws.root.as_path()).unwrap();
    assert_eq!(count, 0, "no-change recompute should process zero symbols");

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

// sutra/471: in tree-sitter-dart the root node of a method (method_declaration
// wrapping method_signature) differs in kind and nesting from a top-level
// function (function_declaration), so an extracted helper with the same body
// must still score close to the method it came from.
#[test]
fn dart_method_and_top_level_function_with_same_body_are_similar() {
    let f = setup(&[(
        "lib/a.dart",
        concat!(
            "class Foo {\n",
            "  void showInvalid(int x) {\n",
            "    if (x > 0) {\n",
            "      print(x + 1);\n",
            "    }\n",
            "  }\n",
            "}\n",
            "void showInvalidEntry(int x) {\n",
            "  if (x > 0) {\n",
            "    print(x + 1);\n",
            "  }\n",
            "}\n",
        ),
    )]);
    let mut f = f;
    f.ws.languages = vec!["dart".to_string()];
    parse(&f);

    let id_of = |name: &str| -> i64 {
        f.db.conn_for_test()
            .query_row(
                "SELECT id FROM symbols WHERE short_name = ?1 AND kind IN ('function', 'method')",
                [name],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("no symbol {name}: {e}"))
    };
    let (method, func) = (id_of("showInvalid"), id_of("showInvalidEntry"));
    let vecs = load_vectors(&f.db);
    for mode in ["strip", "embed"] {
        let sim = get_vec(&vecs, method, mode).cosine_similarity(get_vec(&vecs, func, mode));
        assert!(sim > 0.8, "{mode}: method vs function sim={sim:.4}");
    }
}

// sutra/503: siblings were permuted only by absolute position, so one
// statement inserted at the top of a block shifted every later sibling into an
// unrelated subspace. A guard added to an otherwise identical body must keep
// the two functions close, while reordering must still change the vector.
#[test]
fn leading_statement_insertion_keeps_functions_similar() {
    let f = setup(&[(
        "src/lib.rs",
        concat!(
            "fn guarded(x: i32) {\n",
            "    if x < 0 { return; }\n",
            "    let y = x + 1;\n",
            "    log(y);\n",
            "    store(y * 2);\n",
            "    notify(y, x);\n",
            "}\n",
            "fn plain(x: i32) {\n",
            "    let y = x + 1;\n",
            "    log(y);\n",
            "    store(y * 2);\n",
            "    notify(y, x);\n",
            "}\n",
            "fn reordered(x: i32) {\n",
            "    notify(y, x);\n",
            "    store(y * 2);\n",
            "    log(y);\n",
            "    let y = x + 1;\n",
            "}\n",
        ),
    )]);
    parse(&f);

    let id_of = |name: &str| -> i64 {
        f.db.conn_for_test()
            .query_row(
                "SELECT id FROM symbols WHERE short_name = ?1 AND kind = 'function'",
                [name],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("no symbol {name}: {e}"))
    };
    let (guarded, plain, reordered) = (id_of("guarded"), id_of("plain"), id_of("reordered"));
    let vecs = load_vectors(&f.db);
    for mode in ["strip", "embed"] {
        let inserted = get_vec(&vecs, guarded, mode).cosine_similarity(get_vec(&vecs, plain, mode));
        assert!(
            inserted > 0.55,
            "{mode}: guarded vs plain sim={inserted:.4}"
        );
    }
    // Stripped, these statements are near-identical call shapes, so order is
    // only observable once identifiers are bound in.
    let swapped =
        get_vec(&vecs, reordered, "embed").cosine_similarity(get_vec(&vecs, plain, "embed"));
    assert!(swapped < 0.9, "embed: reordered vs plain sim={swapped:.4}");
}

/// The default answers "does this already exist" (sutra/484): a renamed copy
/// ranks first and is marked a likely duplicate, while a function that only
/// shares its shape, which strip scores as high, is not.
#[test]
fn default_mode_ranks_the_copy_above_a_same_shape_function() {
    let f = setup(&[(
        "src/lib.rs",
        concat!(
            "pub fn load_waivers(conn: &Conn, rule: &str) -> Vec<Waiver> {\n",
            "    let mut stmt = conn.prepare(WAIVER_SELECT).expect(\"invariant: static sql\");\n",
            "    let rows = stmt.query_map([rule], waiver_from_row).expect(\"invariant: bound\");\n",
            "    let waivers: Vec<Waiver> = rows.filter_map(|w| w.ok()).collect();\n",
            "    waivers.into_iter().filter(|w| !w.expired()).collect()\n",
            "}\n",
            "pub fn active_waivers(conn: &Conn, rule_id: &str) -> Vec<Waiver> {\n",
            "    let mut stmt = conn.prepare(WAIVER_SELECT).expect(\"invariant: static sql\");\n",
            "    let rows = stmt.query_map([rule_id], waiver_from_row).expect(\"invariant: bound\");\n",
            "    let found: Vec<Waiver> = rows.filter_map(|w| w.ok()).collect();\n",
            "    found.into_iter().filter(|w| !w.expired()).collect()\n",
            "}\n",
            "pub fn render_rows(page: &Page, title: &str) -> Vec<Line> {\n",
            "    let mut out = page.header(title).expect(\"invariant: header fits\");\n",
            "    let lines = out.wrap_all([title], line_from_cell).expect(\"invariant: width\");\n",
            "    let shown: Vec<Line> = lines.filter_map(|l| l.ok()).collect();\n",
            "    shown.into_iter().filter(|l| !l.blank()).collect()\n",
            "}\n",
        ),
    )]);
    parse(&f);

    let result = similar(&f, "active_waivers", None, Some(10), Some(0.0));
    assert_eq!(result["mode"], "dup");
    let matches = result["matches"].as_array().unwrap();
    assert_eq!(matches[0]["symbol"], "load_waivers", "{result}");
    assert_eq!(matches[0]["likely_duplicate"], true, "{result}");
    let render = matches
        .iter()
        .find(|m| m["symbol"] == "render_rows")
        .expect("the same-shape function is ranked, below the copy");
    assert_eq!(render["likely_duplicate"], false, "{result}");
}

/// A match that fires on a rare shared code run alone is returned however
/// high the threshold, and marked, though its similarity is below it: the
/// threshold filters only the matches the review would not report (sutra/511).
#[test]
fn dup_mode_keeps_a_block_match_below_the_threshold() {
    let f = setup(&[(
        "src/lib.rs",
        concat!(
            "pub fn sync_ledger(store: &Store, cutoff: u64) -> usize {\n",
            "    let pending = store.pending_entries(cutoff);\n",
            "    let merged = reconcile_batches(pending.chunks(64).map(Batch::from_slice).collect(), cutoff);\n",
            "    store.commit(merged.len());\n",
            "    merged.len()\n",
            "}\n",
            "pub fn draw_chart(canvas: &mut Canvas, series: &[f32]) {\n",
            "    canvas.clear(Color::WHITE);\n",
            "    let axis = Axis::fit(series.iter().copied().fold(0.0, f32::max));\n",
            "    let merged = reconcile_batches(pending.chunks(64).map(Batch::from_slice).collect(), cutoff);\n",
            "    canvas.plot(&axis, series);\n",
            "    canvas.legend(\"load\");\n",
            "}\n",
        ),
    )]);
    parse(&f);

    let result = similar(&f, "draw_chart", None, Some(10), Some(0.99));
    let matches = result["matches"].as_array().unwrap();
    let ledger = matches
        .iter()
        .find(|m| m["symbol"] == "sync_ledger")
        .unwrap_or_else(|| panic!("the block match is kept: {result}"));
    assert!(ledger["similarity"].as_f64().unwrap() < 0.5, "{result}");
    assert!(ledger["shared_runs"].as_u64().unwrap() >= 6, "{result}");
    assert_eq!(ledger["likely_duplicate"], true, "{result}");
    assert!(
        matches.iter().all(|m| m["likely_duplicate"] == true),
        "only firing matches pass a 0.99 threshold: {result}"
    );
}

/// The review never checks a function under five lines or a test, so a
/// query it would not check has no likely duplicate, even where the match
/// clears the firing bar (sutra/511).
#[test]
fn dup_mode_marks_nothing_for_a_query_the_review_skips() {
    let body = concat!(
        "    let mut stmt = conn.prepare(WAIVER_SELECT).expect(\"invariant: static sql\");\n",
        "    let rows = stmt.query_map([rule], waiver_from_row).expect(\"invariant: bound\");\n",
        "    let waivers: Vec<Waiver> = rows.filter_map(|w| w.ok()).collect();\n",
        "    waivers.into_iter().filter(|w| !w.expired()).collect()\n",
    );
    let source = format!(
        "pub fn count_waivers(conn: &Conn, rule: &str) -> usize {{\n\
         \x20   let mut stmt = conn.prepare(WAIVER_SELECT).expect(\"invariant: static sql\");\n\
         \x20   stmt.query_map([rule], waiver_from_row).expect(\"invariant: bound\").count()\n}}\n\
         pub fn load_waivers(conn: &Conn, rule: &str) -> Vec<Waiver> {{\n{body}}}\n\
         #[test]\n\
         fn loads_waivers() {{\n{body}}}\n"
    );
    let f = setup(&[("src/lib.rs", &source)]);
    parse(&f);

    for query in ["count_waivers", "loads_waivers"] {
        let result = similar(&f, query, None, Some(10), Some(0.0));
        let m = result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["symbol"] == "load_waivers")
            .unwrap_or_else(|| panic!("{query}: load_waivers is ranked: {result}"));
        assert!(
            m["shared_runs"].as_u64().unwrap() >= 6 || m["similarity"].as_f64().unwrap() >= 0.5,
            "{query}: the match clears the firing bar: {result}"
        );
        assert_eq!(m["likely_duplicate"], false, "{query}: {result}");
    }
}
