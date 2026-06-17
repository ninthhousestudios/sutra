use rusqlite::params;
use sutra::db::{Db, InsertSymbolParams};
use sutra::lessons::LessonsDb;
use sutra::tools::remember::{LocationAnchor, RememberArgs};

fn setup() -> (tempfile::TempDir, Db, tempfile::TempDir, LessonsDb) {
    let ws_dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test", ws_dir.path()).unwrap();
    let lessons_dir = tempfile::tempdir().unwrap();
    let lessons_db = LessonsDb::open(lessons_dir.path()).unwrap();
    (ws_dir, db, lessons_dir, lessons_db)
}

fn seed_file_with_imports(db: &Db) -> i64 {
    let file_id = db
        .upsert_file("src/db/mod.rs", "rust", "abc", 100, true)
        .unwrap();
    db.insert_import(file_id, "rusqlite::params", None, 1)
        .unwrap();
    db.insert_import(file_id, "rusqlite::Connection", None, 2)
        .unwrap();
    db.insert_import(file_id, "serde_json::json", None, 3)
        .unwrap();
    file_id
}

fn seed_symbol(db: &Db, file_id: i64, qn: &str, sn: &str) {
    db.insert_symbol(&InsertSymbolParams {
        file_id,
        qualified_name: qn,
        short_name: sn,
        kind: "function",
        signature: None,
        signature_hash: None,
        visibility: None,
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
    .unwrap();
}

fn store_args(text: &str, anchors: Vec<LocationAnchor>) -> RememberArgs {
    RememberArgs {
        text: Some(text.to_string()),
        location_anchors: Some(anchors),
        source_tasks: None,
        project_origin: None,
        categories: None,
        cite: None,
        anti_verify: None,
        workspace: None,
    }
}

fn anchor(kind: &str, value: &str) -> LocationAnchor {
    LocationAnchor {
        kind: kind.to_string(),
        value: value.to_string(),
    }
}

fn stored_anchors(lessons_db: &LessonsDb, lesson_id: &str) -> Vec<(String, String)> {
    let conn = lessons_db.conn_for_test();
    let mut stmt = conn
        .prepare("SELECT kind, value FROM anchors WHERE lesson_id = ?1 ORDER BY kind, value")
        .unwrap();
    stmt.query_map(params![lesson_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

fn stored_categories(lessons_db: &LessonsDb, lesson_id: &str) -> Vec<String> {
    let conn = lessons_db.conn_for_test();
    let mut stmt = conn
        .prepare("SELECT tag FROM categories WHERE lesson_id = ?1 ORDER BY tag")
        .unwrap();
    stmt.query_map(params![lesson_id], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

#[test]
fn symbol_anchor_enriches_imports_directory_language() {
    let (_wd, db, _ld, lessons_db) = setup();
    let file_id = seed_file_with_imports(&db);
    seed_symbol(&db, file_id, "Db::store", "store");

    let args = store_args("test lesson", vec![anchor("symbol", "Db::store")]);
    let result = sutra::tools::remember::handle(&lessons_db, Some(&db), &args).unwrap();

    let id = result["lesson_id"].as_str().unwrap();
    assert!(result["enriched"].as_bool().unwrap());

    let anchors = stored_anchors(&lessons_db, id);
    assert!(
        anchors.contains(&("symbol".into(), "Db::store".into())),
        "explicit symbol anchor preserved"
    );
    assert!(
        anchors.contains(&("directory".into(), "src/db".into())),
        "directory anchor inferred: {anchors:?}"
    );
    assert!(
        anchors.contains(&("import_pattern".into(), "rusqlite::*".into())),
        "rusqlite import pattern inferred: {anchors:?}"
    );
    assert!(
        anchors.contains(&("import_pattern".into(), "serde_json::*".into())),
        "serde_json import pattern inferred: {anchors:?}"
    );
    // rusqlite appears twice in imports but should only produce one anchor
    assert_eq!(
        anchors
            .iter()
            .filter(|(k, v)| k == "import_pattern" && v == "rusqlite::*")
            .count(),
        1,
        "deduped import anchors"
    );

    let cats = stored_categories(&lessons_db, id);
    assert!(
        cats.contains(&"rust".to_string()),
        "language category: {cats:?}"
    );
    assert!(
        cats.contains(&"sqlite".to_string()),
        "technology category from rusqlite: {cats:?}"
    );
    assert!(
        cats.contains(&"json".to_string()),
        "technology category from serde_json: {cats:?}"
    );
}

#[test]
fn file_anchor_enriches_directory_and_language() {
    let (_wd, db, _ld, lessons_db) = setup();
    db.upsert_file("lib/src/widgets/button.dart", "dart", "xyz", 50, true)
        .unwrap();

    let args = store_args(
        "dart widget lesson",
        vec![anchor("file", "lib/src/widgets/button.dart")],
    );
    let result = sutra::tools::remember::handle(&lessons_db, Some(&db), &args).unwrap();
    let id = result["lesson_id"].as_str().unwrap();

    let anchors = stored_anchors(&lessons_db, id);
    assert!(anchors.contains(&("directory".into(), "lib/src/widgets".into())));
    assert!(anchors.contains(&("file".into(), "lib/src/widgets/button.dart".into())));

    let cats = stored_categories(&lessons_db, id);
    assert!(cats.contains(&"dart".to_string()));
}

#[test]
fn explicit_categories_preserved_alongside_enriched() {
    let (_wd, db, _ld, lessons_db) = setup();
    let file_id = seed_file_with_imports(&db);
    seed_symbol(&db, file_id, "Db::store", "store");

    let mut args = store_args("test", vec![anchor("symbol", "Db::store")]);
    args.categories = Some(vec!["concurrency".into(), "rust".into()]);

    let result = sutra::tools::remember::handle(&lessons_db, Some(&db), &args).unwrap();
    let id = result["lesson_id"].as_str().unwrap();

    let cats = stored_categories(&lessons_db, id);
    assert!(
        cats.contains(&"concurrency".to_string()),
        "explicit preserved"
    );
    assert!(
        cats.contains(&"rust".to_string()),
        "explicit rust not duplicated"
    );
    assert!(cats.contains(&"sqlite".to_string()), "enriched tech added");
    // "rust" should appear exactly once
    assert_eq!(
        cats.iter().filter(|c| c.as_str() == "rust").count(),
        1,
        "no duplicate categories"
    );
}

#[test]
fn no_workspace_db_stores_without_enrichment() {
    let (_ld, lessons_db) = {
        let dir = tempfile::tempdir().unwrap();
        let db = LessonsDb::open(dir.path()).unwrap();
        (dir, db)
    };

    let args = store_args("plain lesson", vec![anchor("symbol", "foo")]);
    let result = sutra::tools::remember::handle(&lessons_db, None, &args).unwrap();

    let id = result["lesson_id"].as_str().unwrap();
    assert!(!result["enriched"].as_bool().unwrap());

    let anchors = stored_anchors(&lessons_db, id);
    assert_eq!(anchors.len(), 1);
    assert_eq!(anchors[0], ("symbol".into(), "foo".into()));
}

#[test]
fn symbol_not_found_stores_explicit_only() {
    let (_wd, db, _ld, lessons_db) = setup();

    let args = store_args(
        "lesson about missing symbol",
        vec![anchor("symbol", "NonExistent::thing")],
    );
    let result = sutra::tools::remember::handle(&lessons_db, Some(&db), &args).unwrap();

    let id = result["lesson_id"].as_str().unwrap();
    assert!(result["enriched"].as_bool().unwrap());

    let anchors = stored_anchors(&lessons_db, id);
    assert_eq!(anchors.len(), 1, "only explicit anchor stored");
    assert_eq!(anchors[0], ("symbol".into(), "NonExistent::thing".into()));
}

#[test]
fn duplicate_file_across_anchors_deduplicates_enrichment() {
    let (_wd, db, _ld, lessons_db) = setup();
    let file_id = seed_file_with_imports(&db);
    seed_symbol(&db, file_id, "Db::store", "store");
    seed_symbol(&db, file_id, "Db::query", "query");

    let args = store_args(
        "lesson about two symbols in same file",
        vec![anchor("symbol", "Db::store"), anchor("symbol", "Db::query")],
    );
    let result = sutra::tools::remember::handle(&lessons_db, Some(&db), &args).unwrap();
    let id = result["lesson_id"].as_str().unwrap();

    let anchors = stored_anchors(&lessons_db, id);
    // Directory should appear only once despite two symbols in same file
    assert_eq!(
        anchors.iter().filter(|(k, _)| k == "directory").count(),
        1,
        "directory deduped: {anchors:?}"
    );
    // Import patterns should be deduped too
    assert_eq!(
        anchors
            .iter()
            .filter(|(k, v)| k == "import_pattern" && v == "rusqlite::*")
            .count(),
        1,
        "import pattern deduped: {anchors:?}"
    );
}

#[test]
fn dart_package_imports_enriched() {
    let (_wd, db, _ld, lessons_db) = setup();
    let file_id = db
        .upsert_file("lib/src/app.dart", "dart", "d1", 30, true)
        .unwrap();
    db.insert_import(file_id, "package:flutter/material.dart", None, 1)
        .unwrap();
    db.insert_import(file_id, "dart:core", None, 2).unwrap();
    seed_symbol(&db, file_id, "MyApp", "MyApp");

    let args = store_args("flutter lesson", vec![anchor("symbol", "MyApp")]);
    let result = sutra::tools::remember::handle(&lessons_db, Some(&db), &args).unwrap();
    let id = result["lesson_id"].as_str().unwrap();

    let anchors = stored_anchors(&lessons_db, id);
    assert!(
        anchors.contains(&("import_pattern".into(), "flutter::*".into())),
        "dart package import extracted: {anchors:?}"
    );
    // dart:core should be skipped (stdlib)
    assert!(
        !anchors.iter().any(|(_, v)| v.contains("dart")),
        "dart stdlib skipped: {anchors:?}"
    );

    let cats = stored_categories(&lessons_db, id);
    assert!(cats.contains(&"dart".to_string()));
    assert!(cats.contains(&"flutter".to_string()));
}

#[test]
fn rust_internal_imports_skipped() {
    let (_wd, db, _ld, lessons_db) = setup();
    let file_id = db
        .upsert_file("src/db/mod.rs", "rust", "h1", 50, true)
        .unwrap();
    db.insert_import(file_id, "crate::error::Result", None, 1)
        .unwrap();
    db.insert_import(file_id, "self::helpers::parse", None, 2)
        .unwrap();
    db.insert_import(file_id, "super::config::Config", None, 3)
        .unwrap();
    db.insert_import(file_id, "std::collections::HashMap", None, 4)
        .unwrap();
    db.insert_import(file_id, "rusqlite::Connection", None, 5)
        .unwrap();
    seed_symbol(&db, file_id, "Db::open", "open");

    let args = store_args("internal import lesson", vec![anchor("symbol", "Db::open")]);
    let result = sutra::tools::remember::handle(&lessons_db, Some(&db), &args).unwrap();
    let id = result["lesson_id"].as_str().unwrap();

    let anchors = stored_anchors(&lessons_db, id);
    // crate, self, super, std should NOT produce import anchors
    assert!(
        !anchors.iter().any(|(_, v)| v.starts_with("crate::")),
        "crate:: skipped: {anchors:?}"
    );
    assert!(
        !anchors.iter().any(|(_, v)| v.starts_with("self::")),
        "self:: skipped: {anchors:?}"
    );
    assert!(
        !anchors.iter().any(|(_, v)| v.starts_with("super::")),
        "super:: skipped: {anchors:?}"
    );
    assert!(
        !anchors.iter().any(|(_, v)| v.starts_with("std::")),
        "std:: skipped: {anchors:?}"
    );
    // rusqlite should still be enriched
    assert!(
        anchors.contains(&("import_pattern".into(), "rusqlite::*".into())),
        "external import preserved: {anchors:?}"
    );
}

#[test]
fn ambiguous_short_symbol_skips_enrichment() {
    let (_wd, db, _ld, lessons_db) = setup();
    let file_a = db.upsert_file("src/db.rs", "rust", "ha", 50, true).unwrap();
    let file_b = db
        .upsert_file("lib/store.dart", "dart", "hb", 30, true)
        .unwrap();
    seed_symbol(&db, file_a, "Db::store", "store");
    seed_symbol(&db, file_b, "Store::store", "store");

    // Short name "store" is ambiguous — enrichment should not pick arbitrarily
    let args = store_args("ambiguous lesson", vec![anchor("symbol", "store")]);
    let result = sutra::tools::remember::handle(&lessons_db, Some(&db), &args).unwrap();
    let id = result["lesson_id"].as_str().unwrap();

    let anchors = stored_anchors(&lessons_db, id);
    assert_eq!(
        anchors.len(),
        1,
        "only explicit anchor stored when symbol is ambiguous: {anchors:?}"
    );
    assert_eq!(anchors[0], ("symbol".into(), "store".into()));

    let cats = stored_categories(&lessons_db, id);
    assert!(
        cats.is_empty(),
        "no inferred categories for ambiguous symbol: {cats:?}"
    );
}
