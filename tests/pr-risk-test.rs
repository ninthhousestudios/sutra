use sutra::db::{Db, InsertSymbolParams};
use sutra::tools::pr_risk;

fn sym<'a>(
    file_id: i64,
    qn: &'a str,
    sn: &'a str,
    sig: Option<&'a str>,
    sl: i64,
    el: i64,
    cognitive: Option<i64>,
) -> InsertSymbolParams<'a> {
    InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind: "function",
        signature: sig,
        signature_hash: None,
        visibility: Some("pub"),
        start_line: sl,
        start_col: 0,
        end_line: el,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive,
        flags: 0,
    }
}

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open("test", dir.path()).unwrap();
    (dir, db)
}

fn setup_db_with_files() -> (tempfile::TempDir, Db) {
    let (dir, db) = setup_db();

    db.upsert_file("src/a.rs", "rust", "ha", 100, true).unwrap();
    db.upsert_file("src/b.rs", "rust", "hb", 50, true).unwrap();

    let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
    let fb = db.file_by_path("src/b.rs").unwrap().unwrap();

    db.insert_symbol(&sym(
        fa.id,
        "a::small_fn",
        "small_fn",
        Some("fn small_fn()"),
        1,
        5,
        Some(2),
    ))
    .unwrap();
    db.insert_symbol(&sym(
        fb.id,
        "b::complex_fn",
        "complex_fn",
        Some("fn complex_fn()"),
        1,
        30,
        Some(25),
    ))
    .unwrap();

    // a has low blast, b has high blast
    db.update_rollups(fa.id, 1, 3).unwrap();
    db.update_rollups(fb.id, 5, 30).unwrap();

    (dir, db)
}

#[test]
fn empty_diff_returns_zero_score() {
    let (_dir, db) = setup_db();
    let changed_paths: Vec<String> = vec![];
    let result = pr_risk::compute(&db, &changed_paths, &Default::default()).unwrap();

    let score = result["composite_score"].as_f64().unwrap();
    assert!(
        (score - 0.0).abs() < f64::EPSILON,
        "empty diff should be 0.0, got {score}"
    );
    assert_eq!(result["riskiest_symbols"].as_array().unwrap().len(), 0);
}

#[test]
fn single_low_risk_file_scores_low() {
    let (_dir, db) = setup_db_with_files();
    let changed = vec!["src/a.rs".to_string()];
    let result = pr_risk::compute(&db, &changed, &Default::default()).unwrap();

    let score = result["composite_score"].as_f64().unwrap();
    assert!(
        score < 0.15,
        "single low-risk file should score < 0.15, got {score}"
    );
    assert!(
        score > 0.0,
        "should be non-zero since file has blast_radius=3"
    );

    let signals = &result["signals"];
    assert!(signals["blast_radius"]["score"].as_f64().unwrap() < 0.2);
    assert!(signals["complexity"]["score"].as_f64().unwrap() < 0.2);
    assert_eq!(signals["volume"]["raw"].as_u64().unwrap(), 1);

    let syms = result["riskiest_symbols"].as_array().unwrap();
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0]["symbol"], "a::small_fn");
}

#[test]
fn composite_combines_all_signals() {
    let (_dir, db) = setup_db_with_files();
    let changed = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
    let mut churn = pr_risk::ChurnMap::default();
    churn.counts.insert("src/b.rs".to_string(), 15);
    churn.window_days = 90;

    let result = pr_risk::compute(&db, &changed, &churn).unwrap();

    let score = result["composite_score"].as_f64().unwrap();
    // b.rs has blast=30, cognitive=25, churn=15 — should push score significantly up
    assert!(
        score > 0.3,
        "high-risk file should push composite > 0.3, got {score}"
    );
    assert!(score <= 1.0, "score must be <= 1.0, got {score}");

    let signals = &result["signals"];
    // blast: (3+30)/50 = 0.66
    assert!(signals["blast_radius"]["score"].as_f64().unwrap() > 0.5);
    // complexity: max(2,25)/30 = 0.833
    assert!(signals["complexity"]["score"].as_f64().unwrap() > 0.7);
    // churn: 15/20 = 0.75
    assert!(signals["churn"]["score"].as_f64().unwrap() > 0.5);
}

#[test]
fn riskiest_symbols_ranked_correctly() {
    let (_dir, db) = setup_db_with_files();
    let changed = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
    let result = pr_risk::compute(&db, &changed, &Default::default()).unwrap();

    let syms = result["riskiest_symbols"].as_array().unwrap();
    assert_eq!(syms.len(), 2);
    // b::complex_fn has blast=30 and cognitive=25 — should rank first
    assert_eq!(syms[0]["symbol"], "b::complex_fn");
    assert_eq!(syms[1]["symbol"], "a::small_fn");

    let top_risk = syms[0]["risk_score"].as_f64().unwrap();
    let bot_risk = syms[1]["risk_score"].as_f64().unwrap();
    assert!(
        top_risk > bot_risk,
        "complex_fn should rank higher than small_fn"
    );
}

#[test]
fn weights_documented_with_rationale() {
    let (_dir, db) = setup_db();
    let result = pr_risk::compute(&db, &[], &Default::default()).unwrap();

    let weights = &result["weights"];
    for signal in &["blast_radius", "complexity", "churn", "volume"] {
        let entry = &weights[signal];
        assert!(
            entry["weight"].as_f64().is_some(),
            "missing weight for {signal}"
        );
        assert!(
            entry["rationale"].as_str().is_some(),
            "missing rationale for {signal}"
        );
    }

    let sum: f64 = ["blast_radius", "complexity", "churn", "volume"]
        .iter()
        .map(|s| weights[s]["weight"].as_f64().unwrap())
        .sum();
    assert!(
        (sum - 1.0).abs() < 0.001,
        "weights should sum to 1.0, got {sum}"
    );
}

#[test]
fn score_clamped_to_one() {
    let (dir, db) = setup_db();

    // Create many high-risk files to push all signals to max
    for i in 0..30 {
        let path = format!("src/big_{i}.rs");
        db.upsert_file(&path, "rust", &format!("h{i}"), 500, true)
            .unwrap();
        let f = db.file_by_path(&path).unwrap().unwrap();
        let qn = format!("big_{i}::danger");
        db.insert_symbol(&sym(f.id, &qn, "danger", None, 1, 100, Some(50)))
            .unwrap();
        db.update_rollups(f.id, 20, 80).unwrap();
    }

    let paths: Vec<String> = (0..30).map(|i| format!("src/big_{i}.rs")).collect();
    let mut churn = pr_risk::ChurnMap::default();
    for p in &paths {
        churn.counts.insert(p.clone(), 50);
    }

    let result = pr_risk::compute(&db, &paths, &churn).unwrap();
    let score = result["composite_score"].as_f64().unwrap();
    assert!(score <= 1.0, "score must be clamped to 1.0, got {score}");
    assert!(
        score >= 0.95,
        "extreme risk should be near 1.0, got {score}"
    );
    drop(dir);
}
