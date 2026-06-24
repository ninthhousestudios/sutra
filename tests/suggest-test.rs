use sutra::db::{Db, InsertSymbolParams};
use sutra::tools::read::suggest_symbols;

fn setup_db() -> (tempfile::TempDir, Db) {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", dir.path()).unwrap();
    (dir, db)
}

fn seed_file(db: &Db, path: &str) -> i64 {
    db.upsert_file(path, "rust", "abc123", 100, true).unwrap()
}

fn seed_symbol(db: &Db, file_id: i64, qn: &str, sn: &str, kind: &str) -> i64 {
    db.insert_symbol(&InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind,
        signature: None,
        signature_hash: None,
        visibility: Some("pub"),
        start_line: 1,
        start_col: 0,
        end_line: 10,
        end_col: 0,
        parent_symbol_id: None,
        docstring: None,
        cyclomatic: None,
        cognitive: None,
        max_nesting: None,
        flags: 0,
        language_attrs: None,
    })
    .unwrap()
}

fn seed_corpus(db: &Db) {
    let fid = seed_file(db, "src/db.rs");
    seed_symbol(db, fid, "Db::insert_snapshot", "insert_snapshot", "method");
    seed_symbol(db, fid, "Db::delete_snapshot", "delete_snapshot", "method");
    seed_symbol(db, fid, "Db::find_symbols", "find_symbols", "method");
    seed_symbol(db, fid, "Db::all_files", "all_files", "method");

    let fid2 = seed_file(db, "src/config.rs");
    seed_symbol(db, fid2, "Config", "Config", "struct");
    seed_symbol(db, fid2, "Config::from_env", "from_env", "method");

    let fid3 = seed_file(db, "src/other.rs");
    seed_symbol(
        db,
        fid3,
        "Other::insert_snapshot",
        "insert_snapshot",
        "method",
    );
}

#[test]
fn typo_suggests_close_match() {
    let (_dir, db) = setup_db();
    seed_corpus(&db);
    let suggestions = suggest_symbols(&db, "find_symbls", 5);
    assert!(
        suggestions.iter().any(|s| s.contains("find_symbols")),
        "expected find_symbols in {suggestions:?}"
    );
}

#[test]
fn wrong_verb_suggests_via_shared_component() {
    let (_dir, db) = setup_db();
    seed_corpus(&db);
    let suggestions = suggest_symbols(&db, "save_snapshot", 5);
    assert!(
        suggestions.iter().any(|s| s.contains("insert_snapshot")),
        "expected insert_snapshot in {suggestions:?}"
    );
}

#[test]
fn wrong_prefix_suggests_contained_name() {
    let (_dir, db) = setup_db();
    seed_corpus(&db);
    let suggestions = suggest_symbols(&db, "SutraConfig", 5);
    assert!(
        suggestions.iter().any(|s| s.contains("`Config`")),
        "expected Config in {suggestions:?}"
    );
}

#[test]
fn qualifier_boosts_same_type() {
    let (_dir, db) = setup_db();
    seed_corpus(&db);
    let suggestions = suggest_symbols(&db, "Db::save_snapshot", 5);
    assert!(
        !suggestions.is_empty(),
        "expected suggestions for Db::save_snapshot"
    );
    let first = &suggestions[0];
    assert!(
        first.contains("Db::"),
        "expected Db:: qualified match first, got {first}"
    );
}

#[test]
fn short_query_returns_empty() {
    let (_dir, db) = setup_db();
    seed_corpus(&db);
    assert!(suggest_symbols(&db, "a", 5).is_empty());
}

#[test]
fn no_match_returns_empty() {
    let (_dir, db) = setup_db();
    seed_corpus(&db);
    assert!(suggest_symbols(&db, "xyzzy_nonexistent_qqq", 5).is_empty());
}

#[test]
fn respects_limit() {
    let (_dir, db) = setup_db();
    seed_corpus(&db);
    let suggestions = suggest_symbols(&db, "snapshot", 2);
    assert!(
        suggestions.len() <= 2,
        "got {} suggestions",
        suggestions.len()
    );
}
