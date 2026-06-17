use sutra::lessons::{AnchorKind, LessonsDb, LessonsSearchParams, MatchContext, StoreLessonParams};
use tempfile::tempdir;

fn setup_lessons_db() -> (tempfile::TempDir, LessonsDb) {
    let dir = tempdir().unwrap();
    let db = LessonsDb::open(dir.path()).unwrap();
    (dir, db)
}

fn symbol_ctx<'a>(name: &'a str, project: Option<&'a str>) -> MatchContext<'a> {
    MatchContext {
        symbol_name: name,
        file_path: None,
        imports: &[],
        project,
        workspace_languages: &[],
    }
}

#[test]
fn store_and_retrieve_by_symbol() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Don't use unwrap_or_default on fallible refreshes",
            anchors: &[(AnchorKind::Symbol, "refresh_index")],
            categories: &["rust"],
            source_task_ids: &["sutra/38"],
            project_origin: Some("sutra"),
        })
        .unwrap();
    assert!(!id.is_empty());

    let lessons = db
        .query_for_context(&symbol_ctx("refresh_index", Some("sutra")))
        .unwrap()
        .lessons;
    assert_eq!(lessons.len(), 1);
    assert_eq!(lessons[0].id, id);
    assert!(lessons[0].text.contains("unwrap_or_default"));
    assert!(!lessons[0].verified);
    assert_eq!(lessons[0].confidence, 0);
    assert_eq!(lessons[0].project_origin.as_deref(), Some("sutra"));
}

#[test]
fn no_match_returns_empty() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Some lesson",
        anchors: &[(AnchorKind::Symbol, "foo_bar")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let lessons = db
        .query_for_context(&symbol_ctx("completely_different", None))
        .unwrap()
        .lessons;
    assert!(lessons.is_empty());
}

#[test]
fn archived_lessons_not_surfaced() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Old lesson",
            anchors: &[(AnchorKind::Symbol, "some_fn")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    {
        let conn = db.conn_for_test();
        conn.execute(
            "UPDATE lessons SET archived = 1 WHERE id = ?1",
            rusqlite::params![id],
        )
        .unwrap();
    }

    let lessons = db
        .query_for_context(&symbol_ctx("some_fn", None))
        .unwrap()
        .lessons;
    assert!(lessons.is_empty());
}

#[test]
fn multiple_anchors_on_one_lesson() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Applies to both",
        anchors: &[(AnchorKind::Symbol, "fn_a"), (AnchorKind::Symbol, "fn_b")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    assert_eq!(
        db.query_for_context(&symbol_ctx("fn_a", None))
            .unwrap()
            .lessons
            .len(),
        1
    );
    assert_eq!(
        db.query_for_context(&symbol_ctx("fn_b", None))
            .unwrap()
            .lessons
            .len(),
        1
    );
    assert_eq!(
        db.query_for_context(&symbol_ctx("fn_c", None))
            .unwrap()
            .lessons
            .len(),
        0
    );
}

#[test]
fn wal_mode_enabled() {
    let (_dir, db) = setup_lessons_db();
    let mode: String = {
        let conn = db.conn_for_test();
        conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap()
    };
    assert_eq!(mode, "wal");
}

#[test]
fn query_updates_last_surfaced() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Check surfacing timestamp",
            anchors: &[(AnchorKind::Symbol, "target_fn")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    {
        let conn = db.conn_for_test();
        let before: Option<String> = conn
            .query_row(
                "SELECT last_surfaced FROM lessons WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(before.is_none());
    }

    let _ = db
        .query_for_context(&symbol_ctx("target_fn", None))
        .unwrap()
        .lessons;

    {
        let conn = db.conn_for_test();
        let after: Option<String> = conn
            .query_row(
                "SELECT last_surfaced FROM lessons WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(after.is_some());
    }
}

#[test]
fn source_tasks_persisted_as_citations() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Don't use unwrap_or_default on fallible refreshes",
            anchors: &[(AnchorKind::Symbol, "refresh_index")],
            categories: &[],
            source_task_ids: &["sutra/38", "sutra/119"],
            project_origin: Some("sutra"),
        })
        .unwrap();

    let conn = db.conn_for_test();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM citations WHERE lesson_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);

    let task_ids: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT task_id FROM citations WHERE lesson_id = ?1 ORDER BY task_id")
            .unwrap();
        stmt.query_map(rusqlite::params![id], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(task_ids, vec!["sutra/119", "sutra/38"]);
}

#[test]
fn project_scoping_filters_cross_project_lessons() {
    let (_dir, db) = setup_lessons_db();

    db.store(&StoreLessonParams {
        text: "sutra-specific lesson",
        anchors: &[(AnchorKind::Symbol, "init")],
        categories: &[],
        source_task_ids: &[],
        project_origin: Some("sutra"),
    })
    .unwrap();

    db.store(&StoreLessonParams {
        text: "chitta-specific lesson",
        anchors: &[(AnchorKind::Symbol, "init")],
        categories: &[],
        source_task_ids: &[],
        project_origin: Some("chitta"),
    })
    .unwrap();

    db.store(&StoreLessonParams {
        text: "global lesson",
        anchors: &[(AnchorKind::Symbol, "init")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let sutra_lessons = db
        .query_for_context(&symbol_ctx("init", Some("sutra")))
        .unwrap()
        .lessons;
    assert_eq!(sutra_lessons.len(), 2);
    assert!(
        sutra_lessons
            .iter()
            .any(|l| l.text.contains("sutra-specific"))
    );
    assert!(sutra_lessons.iter().any(|l| l.text.contains("global")));
    assert!(
        !sutra_lessons
            .iter()
            .any(|l| l.text.contains("chitta-specific"))
    );

    let chitta_lessons = db
        .query_for_context(&symbol_ctx("init", Some("chitta")))
        .unwrap()
        .lessons;
    assert_eq!(chitta_lessons.len(), 2);
    assert!(
        chitta_lessons
            .iter()
            .any(|l| l.text.contains("chitta-specific"))
    );
    assert!(chitta_lessons.iter().any(|l| l.text.contains("global")));

    let all_lessons = db
        .query_for_context(&symbol_ctx("init", None))
        .unwrap()
        .lessons;
    assert_eq!(all_lessons.len(), 3);
}

// ---------------------------------------------------------------------------
// New: anchor matching tests
// ---------------------------------------------------------------------------

#[test]
fn file_glob_matches_path() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "DB layer lesson",
        anchors: &[(AnchorKind::File, "src/db/*.rs")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let hit = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: Some("src/db/mod.rs"),
            imports: &[],
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert_eq!(hit.len(), 1);
    assert!(hit[0].text.contains("DB layer"));

    let miss = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: Some("src/tools/read.rs"),
            imports: &[],
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert!(miss.is_empty());
}

#[test]
fn import_pattern_matches_imports() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "rusqlite pitfall",
        anchors: &[(AnchorKind::ImportPattern, "rusqlite::*")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let imports_hit = vec![
        "rusqlite::params".to_string(),
        "serde_json::json".to_string(),
    ];
    let hit = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: None,
            imports: &imports_hit,
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert_eq!(hit.len(), 1);
    assert!(hit[0].text.contains("rusqlite pitfall"));

    let imports_miss = vec!["serde_json::json".to_string()];
    let miss = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: None,
            imports: &imports_miss,
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert!(miss.is_empty());
}

#[test]
fn import_pattern_matches_dart_package_imports() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "flutter widget pitfall",
        anchors: &[(AnchorKind::ImportPattern, "flutter::*")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let dart_imports = vec!["package:flutter/material.dart".to_string()];
    let hit = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: None,
            imports: &dart_imports,
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert_eq!(
        hit.len(),
        1,
        "flutter::* should match package:flutter/material.dart"
    );

    let miss_imports = vec!["package:provider/provider.dart".to_string()];
    let miss = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: None,
            imports: &miss_imports,
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert!(
        miss.is_empty(),
        "flutter::* should not match package:provider/..."
    );
}

#[test]
fn directory_anchor_matches_files_under_dir() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "DB directory lesson",
        anchors: &[(AnchorKind::Directory, "src/db")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let hit = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: Some("src/db/mod.rs"),
            imports: &[],
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert_eq!(hit.len(), 1);

    let miss_other = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: Some("src/tools/read.rs"),
            imports: &[],
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert!(miss_other.is_empty());

    // "src/dba/foo.rs" must NOT match "src/db" — no false prefix
    let miss_prefix = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: Some("src/dba/foo.rs"),
            imports: &[],
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert!(miss_prefix.is_empty());
}

#[test]
fn multi_anchor_or_semantics() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "OR semantics lesson",
        anchors: &[
            (AnchorKind::Symbol, "nonexistent_sym"),
            (AnchorKind::File, "src/*.rs"),
        ],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    // Symbol doesn't match, but file glob does — should still surface
    let hit = db
        .query_for_context(&MatchContext {
            symbol_name: "other_sym",
            file_path: Some("src/main.rs"),
            imports: &[],
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert_eq!(hit.len(), 1);
    assert!(hit[0].text.contains("OR semantics"));
}

#[test]
fn no_false_positives_across_kinds() {
    let (_dir, db) = setup_lessons_db();

    db.store(&StoreLessonParams {
        text: "file-anchored",
        anchors: &[(AnchorKind::File, "src/db/*.rs")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    db.store(&StoreLessonParams {
        text: "import-anchored",
        anchors: &[(AnchorKind::ImportPattern, "tokio::*")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    db.store(&StoreLessonParams {
        text: "dir-anchored",
        anchors: &[(AnchorKind::Directory, "tests")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    // Context that matches none of the above
    let imports = vec!["serde::Serialize".to_string()];
    let results = db
        .query_for_context(&MatchContext {
            symbol_name: "unrelated",
            file_path: Some("src/mcp.rs"),
            imports: &imports,
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert!(results.is_empty());
}

#[test]
fn symbol_short_name_match() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Short name lesson",
        anchors: &[(AnchorKind::Symbol, "query_for_context")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    // Query with qualified name should match anchor stored as short name
    let hit = db
        .query_for_context(&symbol_ctx("LessonsDb::query_for_context", None))
        .unwrap()
        .lessons;
    assert_eq!(hit.len(), 1);
    assert!(hit[0].text.contains("Short name"));

    // Direct short name still works
    let direct = db
        .query_for_context(&symbol_ctx("query_for_context", None))
        .unwrap()
        .lessons;
    assert_eq!(direct.len(), 1);
}

#[test]
fn symbol_and_file_deduplicates() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Dedup lesson",
        anchors: &[
            (AnchorKind::Symbol, "my_fn"),
            (AnchorKind::File, "src/*.rs"),
        ],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    // Both symbol and file match — lesson should appear exactly once
    let results = db
        .query_for_context(&MatchContext {
            symbol_name: "my_fn",
            file_path: Some("src/main.rs"),
            imports: &[],
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert_eq!(results.len(), 1);
}

#[test]
fn directory_with_trailing_slash() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Trailing slash lesson",
        anchors: &[(AnchorKind::Directory, "src/db/")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let hit = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: Some("src/db/mod.rs"),
            imports: &[],
            project: None,
            workspace_languages: &[],
        })
        .unwrap()
        .lessons;
    assert_eq!(hit.len(), 1);
}

// ---------------------------------------------------------------------------
// FTS5 search tests
// ---------------------------------------------------------------------------

fn search_params<'a>() -> LessonsSearchParams<'a> {
    LessonsSearchParams {
        query: None,
        category: None,
        symbol: None,
        verified: None,
        project: None,
        include_archived: false,
        limit: 50,
    }
}

#[test]
fn fts5_text_search() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Always wrap SQLite mutations in transactions",
        anchors: &[(AnchorKind::Symbol, "store")],
        categories: &["sqlite"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();
    db.store(&StoreLessonParams {
        text: "Rust lifetime errors usually mean a borrow outlives its scope",
        anchors: &[(AnchorKind::Symbol, "parse")],
        categories: &["rust"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let results = db
        .search(&LessonsSearchParams {
            query: Some("SQLite transactions"),
            ..search_params()
        })
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].text.contains("SQLite"));

    let results = db
        .search(&LessonsSearchParams {
            query: Some("lifetime borrow"),
            ..search_params()
        })
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].text.contains("lifetime"));
}

#[test]
fn fts5_ranking_prefers_better_match() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "A brief note: thread safety matters when writing concurrent code",
        anchors: &[],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();
    db.store(&StoreLessonParams {
        text: "Thread safety for thread pool access",
        anchors: &[],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    // FTS5 uses implicit AND — both docs contain "thread" and "safety"
    let results = db
        .search(&LessonsSearchParams {
            query: Some("thread safety"),
            ..search_params()
        })
        .unwrap();
    assert_eq!(results.len(), 2);
    // BM25 ranks the shorter doc (higher term density) first
    assert!(results[0].text.contains("thread pool"));
}

#[test]
fn search_filter_by_category() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Rust lesson one",
        anchors: &[],
        categories: &["rust"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();
    db.store(&StoreLessonParams {
        text: "SQLite lesson one",
        anchors: &[],
        categories: &["sqlite"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let results = db
        .search(&LessonsSearchParams {
            category: Some("rust"),
            ..search_params()
        })
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].text.contains("Rust"));
}

#[test]
fn search_filter_by_symbol() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Lesson about store",
        anchors: &[(AnchorKind::Symbol, "store")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();
    db.store(&StoreLessonParams {
        text: "Lesson about parse",
        anchors: &[(AnchorKind::Symbol, "parse")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let results = db
        .search(&LessonsSearchParams {
            symbol: Some("store"),
            ..search_params()
        })
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].text.contains("store"));
}

#[test]
fn search_filter_by_verified() {
    let (_dir, db) = setup_lessons_db();
    let verified_id = db
        .store(&StoreLessonParams {
            text: "Verified lesson",
            anchors: &[],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();
    db.store(&StoreLessonParams {
        text: "Unverified lesson",
        anchors: &[],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    {
        let conn = db.conn_for_test();
        conn.execute(
            "UPDATE lessons SET verified = 1 WHERE id = ?1",
            rusqlite::params![verified_id],
        )
        .unwrap();
    }

    let results = db
        .search(&LessonsSearchParams {
            verified: Some(true),
            ..search_params()
        })
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].text.contains("Verified"));
}

#[test]
fn search_combined_filters() {
    let (_dir, db) = setup_lessons_db();
    let target_id = db
        .store(&StoreLessonParams {
            text: "Always use WAL mode for SQLite write performance",
            anchors: &[(AnchorKind::Symbol, "open")],
            categories: &["sqlite"],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();
    db.store(&StoreLessonParams {
        text: "WAL mode helps concurrent readers",
        anchors: &[(AnchorKind::Symbol, "open")],
        categories: &["rust"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();
    db.store(&StoreLessonParams {
        text: "SQLite WAL is default in our codebase",
        anchors: &[(AnchorKind::Symbol, "init")],
        categories: &["sqlite"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    {
        let conn = db.conn_for_test();
        conn.execute(
            "UPDATE lessons SET verified = 1 WHERE id = ?1",
            rusqlite::params![target_id],
        )
        .unwrap();
    }

    // text + category + symbol + verified — only the target lesson matches all four
    let results = db
        .search(&LessonsSearchParams {
            query: Some("WAL"),
            category: Some("sqlite"),
            symbol: Some("open"),
            verified: Some(true),
            ..search_params()
        })
        .unwrap();
    assert_eq!(results.len(), 1);
    assert!(results[0].text.contains("write performance"));
}

#[test]
fn search_empty_results() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Some lesson",
        anchors: &[],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let results = db
        .search(&LessonsSearchParams {
            query: Some("nonexistent xyzzy foobarbaz"),
            ..search_params()
        })
        .unwrap();
    assert!(results.is_empty());
}

#[test]
fn search_no_filters_returns_all() {
    let (_dir, db) = setup_lessons_db();
    for i in 0..3 {
        db.store(&StoreLessonParams {
            text: &format!("Lesson number {i}"),
            anchors: &[],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();
    }

    let results = db.search(&search_params()).unwrap();
    assert_eq!(results.len(), 3);
}

// ---------------------------------------------------------------------------
// Category filtering tests
// ---------------------------------------------------------------------------

fn lang_ctx<'a>(name: &'a str, workspace_languages: &'a [String]) -> MatchContext<'a> {
    MatchContext {
        symbol_name: name,
        file_path: None,
        imports: &[],
        project: None,
        workspace_languages,
    }
}

#[test]
fn category_filtering_excludes_wrong_language() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Rust borrow checker pitfall",
        anchors: &[(AnchorKind::Symbol, "process")],
        categories: &["rust"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let dart_ws = vec!["dart".to_string()];
    let results = db
        .query_for_context(&lang_ctx("process", &dart_ws))
        .unwrap()
        .lessons;
    assert!(
        results.is_empty(),
        "rust-only lesson should not surface in dart workspace"
    );

    let rust_ws = vec!["rust".to_string()];
    let results = db
        .query_for_context(&lang_ctx("process", &rust_ws))
        .unwrap()
        .lessons;
    assert_eq!(
        results.len(),
        1,
        "rust lesson should surface in rust workspace"
    );
}

#[test]
fn uncategorized_lessons_surface_everywhere() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Universal lesson with no categories",
        anchors: &[(AnchorKind::Symbol, "init")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let dart_ws = vec!["dart".to_string()];
    let results = db
        .query_for_context(&lang_ctx("init", &dart_ws))
        .unwrap()
        .lessons;
    assert_eq!(
        results.len(),
        1,
        "uncategorized lesson should surface in any workspace"
    );

    let rust_ws = vec!["rust".to_string()];
    let results = db
        .query_for_context(&lang_ctx("init", &rust_ws))
        .unwrap()
        .lessons;
    assert_eq!(results.len(), 1);
}

#[test]
fn technology_category_surfaces_in_all_workspaces() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "SQLite WAL mode lesson",
        anchors: &[(AnchorKind::Symbol, "open_db")],
        categories: &["sqlite"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let dart_ws = vec!["dart".to_string()];
    let results = db
        .query_for_context(&lang_ctx("open_db", &dart_ws))
        .unwrap()
        .lessons;
    assert_eq!(
        results.len(),
        1,
        "technology category should surface in any workspace"
    );

    let rust_ws = vec!["rust".to_string()];
    let results = db
        .query_for_context(&lang_ctx("open_db", &rust_ws))
        .unwrap()
        .lessons;
    assert_eq!(results.len(), 1);
}

#[test]
fn mixed_categories_surface_if_any_relevant() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Rust SQLite write discipline",
        anchors: &[(AnchorKind::Symbol, "store")],
        categories: &["rust", "sqlite"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    // dart workspace: lesson has language tag "rust" which doesn't match → filtered out
    let dart_ws = vec!["dart".to_string()];
    let results = db
        .query_for_context(&lang_ctx("store", &dart_ws))
        .unwrap()
        .lessons;
    assert_eq!(
        results.len(),
        0,
        "lesson with non-matching language tag should be filtered even with technology tag"
    );

    // rust workspace: language tag "rust" matches → surfaces
    let rust_ws = vec!["rust".to_string()];
    let results = db
        .query_for_context(&lang_ctx("store", &rust_ws))
        .unwrap()
        .lessons;
    assert_eq!(
        results.len(),
        1,
        "lesson with matching language tag should surface"
    );
}

#[test]
fn empty_workspace_languages_skips_filtering() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Rust-only lesson",
        anchors: &[(AnchorKind::Symbol, "build")],
        categories: &["rust"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    // Empty workspace_languages → no filtering, everything surfaces
    let results = db
        .query_for_context(&lang_ctx("build", &[]))
        .unwrap()
        .lessons;
    assert_eq!(
        results.len(),
        1,
        "empty workspace_languages should skip category filtering"
    );
}

#[test]
fn category_filtering_with_multiple_language_lessons() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Rust concurrency lesson",
        anchors: &[(AnchorKind::Symbol, "spawn")],
        categories: &["rust"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();
    db.store(&StoreLessonParams {
        text: "Dart async lesson",
        anchors: &[(AnchorKind::Symbol, "spawn")],
        categories: &["dart"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();
    db.store(&StoreLessonParams {
        text: "General concurrency lesson",
        anchors: &[(AnchorKind::Symbol, "spawn")],
        categories: &["concurrency"],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let rust_ws = vec!["rust".to_string()];
    let results = db
        .query_for_context(&lang_ctx("spawn", &rust_ws))
        .unwrap()
        .lessons;
    assert_eq!(results.len(), 2);
    assert!(results.iter().any(|l| l.text.contains("Rust concurrency")));
    assert!(
        results
            .iter()
            .any(|l| l.text.contains("General concurrency"))
    );
    assert!(!results.iter().any(|l| l.text.contains("Dart async")));

    let dart_ws = vec!["dart".to_string()];
    let results = db
        .query_for_context(&lang_ctx("spawn", &dart_ws))
        .unwrap()
        .lessons;
    assert_eq!(results.len(), 2);
    assert!(results.iter().any(|l| l.text.contains("Dart async")));
    assert!(
        results
            .iter()
            .any(|l| l.text.contains("General concurrency"))
    );
    assert!(!results.iter().any(|l| l.text.contains("Rust concurrency")));
}

// ---------------------------------------------------------------------------
// Citation lifecycle
// ---------------------------------------------------------------------------

#[test]
fn cite_increases_confidence() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Always use transactions",
            anchors: &[(AnchorKind::Symbol, "run_migrations")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    let result = db.cite(&id, Some("sutra/158"), None).unwrap();
    assert_eq!(result.new_confidence, 1);
    assert!(!result.verified);
    assert!(!result.crossed_threshold);

    // Citation row recorded
    let conn = db.conn_for_test();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM citations WHERE lesson_id = ?1 AND field = 'cite'",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn cite_crosses_verification_threshold() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Always use transactions",
            anchors: &[],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    let r1 = db.cite(&id, Some("task/1"), None).unwrap();
    assert_eq!(r1.new_confidence, 1);
    assert!(!r1.verified);
    assert!(!r1.crossed_threshold);

    let r2 = db.cite(&id, Some("task/2"), None).unwrap();
    assert_eq!(r2.new_confidence, 2);
    assert!(r2.verified);
    assert!(r2.crossed_threshold);

    // Third cite: still verified, but didn't *cross* this time
    let r3 = db.cite(&id, Some("task/3"), None).unwrap();
    assert_eq!(r3.new_confidence, 3);
    assert!(r3.verified);
    assert!(!r3.crossed_threshold);
}

#[test]
fn anti_verify_drops_confidence() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "lesson",
            anchors: &[],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    db.cite(&id, Some("task/1"), None).unwrap();
    db.cite(&id, Some("task/2"), None).unwrap();
    // Now verified with confidence 2

    let r = db.anti_verify(&id).unwrap();
    assert_eq!(r.new_confidence, 1);
    assert!(!r.verified);
}

#[test]
fn anti_verify_floors_at_zero() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "lesson",
            anchors: &[],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    let r = db.anti_verify(&id).unwrap();
    assert_eq!(r.new_confidence, 0);
    assert!(!r.verified);
}

// ---------------------------------------------------------------------------
// Surfacing priority
// ---------------------------------------------------------------------------

#[test]
fn verified_suppresses_unverified_on_same_anchor() {
    let (_dir, db) = setup_lessons_db();
    let v_id = db
        .store(&StoreLessonParams {
            text: "verified lesson",
            anchors: &[(AnchorKind::Symbol, "target_fn")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();
    db.store(&StoreLessonParams {
        text: "unverified lesson",
        anchors: &[(AnchorKind::Symbol, "target_fn")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    // Verify the first lesson
    db.cite(&v_id, Some("t/1"), None).unwrap();
    db.cite(&v_id, Some("t/2"), None).unwrap();

    let ctx = symbol_ctx("target_fn", None);
    let cl = db.query_for_context(&ctx).unwrap();
    assert_eq!(cl.lessons.len(), 1);
    assert!(cl.lessons[0].text.contains("verified lesson"));
    assert!(!cl.lessons[0].text.contains("[unverified]"));
}

#[test]
fn unverified_surfaces_when_no_verified_on_same_anchor() {
    let (_dir, db) = setup_lessons_db();

    // Verified lesson on anchor "other_fn"
    let v_id = db
        .store(&StoreLessonParams {
            text: "verified on other",
            anchors: &[(AnchorKind::Symbol, "other_fn")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();
    db.cite(&v_id, Some("t/1"), None).unwrap();
    db.cite(&v_id, Some("t/2"), None).unwrap();

    // Unverified lesson on anchor "target_fn"
    db.store(&StoreLessonParams {
        text: "unverified on target",
        anchors: &[(AnchorKind::Symbol, "target_fn")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let ctx = symbol_ctx("target_fn", None);
    let cl = db.query_for_context(&ctx).unwrap();
    assert_eq!(cl.lessons.len(), 1);
    assert!(
        cl.lessons[0]
            .text
            .contains("[unverified] unverified on target")
    );
}

#[test]
fn unverified_tagged_when_no_verified_exist() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "some lesson",
        anchors: &[(AnchorKind::Symbol, "my_fn")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let ctx = symbol_ctx("my_fn", None);
    let cl = db.query_for_context(&ctx).unwrap();
    assert_eq!(cl.lessons.len(), 1);
    assert!(cl.lessons[0].text.starts_with("[unverified] "));
}

#[test]
fn duplicate_cite_is_idempotent() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "lesson",
            anchors: &[],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    let r1 = db.cite(&id, Some("task/1"), None).unwrap();
    assert_eq!(r1.new_confidence, 1);

    // Same task cites again — should be no-op
    let r2 = db.cite(&id, Some("task/1"), None).unwrap();
    assert_eq!(r2.new_confidence, 1);
    assert!(!r2.verified);

    // Different task still works
    let r3 = db.cite(&id, Some("task/2"), None).unwrap();
    assert_eq!(r3.new_confidence, 2);
    assert!(r3.verified);
}

#[test]
fn multi_anchor_no_false_suppression() {
    let (_dir, db) = setup_lessons_db();

    // Verified lesson anchored to symbol "shared_fn" AND file "src/alpha.rs"
    let v_id = db
        .store(&StoreLessonParams {
            text: "verified multi-anchor",
            anchors: &[
                (AnchorKind::Symbol, "shared_fn"),
                (AnchorKind::File, "src/alpha.rs"),
            ],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();
    db.cite(&v_id, Some("t/1"), None).unwrap();
    db.cite(&v_id, Some("t/2"), None).unwrap();

    // Unverified lesson anchored ONLY to file "src/alpha.rs" (non-matching
    // in this context since we query by symbol "other_fn", file "src/beta.rs")
    // but ALSO anchored to symbol "other_fn" which IS the query symbol.
    // It shares file anchor "src/alpha.rs" with the verified lesson, but that
    // anchor didn't match this context — only "symbol:other_fn" matched.
    db.store(&StoreLessonParams {
        text: "unverified different context",
        anchors: &[
            (AnchorKind::Symbol, "other_fn"),
            (AnchorKind::File, "src/alpha.rs"),
        ],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    // Query for "other_fn" in file "src/beta.rs" — the file anchor
    // "src/alpha.rs" does NOT match this context.
    let ctx = MatchContext {
        symbol_name: "other_fn",
        file_path: Some("src/beta.rs"),
        imports: &[],
        project: None,
        workspace_languages: &[],
    };
    let cl = db.query_for_context(&ctx).unwrap();

    // The unverified lesson should surface because its matched anchor
    // (symbol:other_fn) doesn't overlap with the verified lesson's
    // matched anchor (symbol:shared_fn). The shared file anchor
    // "src/alpha.rs" didn't match this context so shouldn't cause
    // suppression.
    let has_unverified = cl
        .lessons
        .iter()
        .any(|l| l.text.contains("[unverified] unverified different context"));
    assert!(
        has_unverified,
        "unverified lesson should not be suppressed when the shared anchor didn't match this context"
    );
}

#[test]
fn cap_prioritizes_verified_over_unverified() {
    let (_dir, db) = setup_lessons_db();

    // Store 12 lessons: 2 verified on different symbols, 10 unverified on
    // different symbols. Query via a file anchor that matches all of them.
    let mut verified_ids = Vec::new();
    for i in 0..2 {
        let id = db
            .store(&StoreLessonParams {
                text: &format!("verified-{i}"),
                anchors: &[(AnchorKind::File, "src/big.rs")],
                categories: &[],
                source_task_ids: &[],
                project_origin: None,
            })
            .unwrap();
        db.cite(&id, Some(&format!("t/{i}a")), None).unwrap();
        db.cite(&id, Some(&format!("t/{i}b")), None).unwrap();
        verified_ids.push(id);
    }
    for i in 0..10 {
        db.store(&StoreLessonParams {
            text: &format!("unverified-{i}"),
            anchors: &[(AnchorKind::File, "src/big.rs")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();
    }

    let ctx = MatchContext {
        symbol_name: "",
        file_path: Some("src/big.rs"),
        imports: &[],
        project: None,
        workspace_languages: &[],
    };
    let cl = db.query_for_context(&ctx).unwrap();

    // Both verified lessons must be in the result (cap is 10)
    let verified_count = cl.lessons.iter().filter(|l| l.verified).count();
    assert_eq!(verified_count, 2, "all verified lessons should survive cap");
    // Verified should come first
    assert!(cl.lessons[0].verified);
    assert!(cl.lessons[1].verified);
}

// ---------------------------------------------------------------------------
// Staleness detection
// ---------------------------------------------------------------------------

#[test]
fn verification_snapshots_content_hash() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Always lock before write",
            anchors: &[(AnchorKind::Symbol, "write_data")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    let resolver = |kind: &str, value: &str| -> Option<String> {
        if kind == "symbol" && value == "write_data" {
            Some("hash_abc123".to_string())
        } else {
            None
        }
    };

    // First cite — no threshold crossing yet
    db.cite(&id, Some("task/1"), Some(&resolver)).unwrap();

    let conn = db.conn_for_test();
    let av_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM anchor_verification WHERE lesson_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(av_count, 0, "no snapshot before verification threshold");

    drop(conn);

    // Second cite — crosses threshold
    let r2 = db.cite(&id, Some("task/2"), Some(&resolver)).unwrap();
    assert!(r2.crossed_threshold);

    let conn = db.conn_for_test();
    let av_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM anchor_verification WHERE lesson_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(av_count, 1, "snapshot created on verification");

    let stored_hash: String = conn
        .query_row(
            "SELECT content_hash FROM anchor_verification WHERE lesson_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_hash, "hash_abc123");

    // verified_at set on lesson
    let verified_at: Option<String> = conn
        .query_row(
            "SELECT verified_at FROM lessons WHERE id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(verified_at.is_some(), "verified_at should be set");
}

#[test]
fn stale_when_content_changed() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Stale test lesson",
            anchors: &[(AnchorKind::Symbol, "my_fn")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    let original_resolver = |kind: &str, value: &str| -> Option<String> {
        if kind == "symbol" && value == "my_fn" {
            Some("original_hash".to_string())
        } else {
            None
        }
    };

    db.cite(&id, Some("task/1"), Some(&original_resolver))
        .unwrap();
    db.cite(&id, Some("task/2"), Some(&original_resolver))
        .unwrap();

    // Content has changed
    let changed_resolver = |kind: &str, value: &str| -> Option<String> {
        if kind == "symbol" && value == "my_fn" {
            Some("changed_hash".to_string())
        } else {
            None
        }
    };

    let mut lessons = db
        .query_for_context(&symbol_ctx("my_fn", None))
        .unwrap()
        .lessons;
    assert_eq!(lessons.len(), 1);
    assert!(lessons[0].verified);
    assert_eq!(lessons[0].stale, None, "stale not yet applied");

    db.apply_staleness(&mut lessons, &changed_resolver).unwrap();
    assert_eq!(lessons[0].stale, Some(true));
}

#[test]
fn not_stale_when_content_unchanged() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Not stale lesson",
            anchors: &[(AnchorKind::Symbol, "stable_fn")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    let resolver = |kind: &str, value: &str| -> Option<String> {
        if kind == "symbol" && value == "stable_fn" {
            Some("same_hash".to_string())
        } else {
            None
        }
    };

    db.cite(&id, Some("task/1"), Some(&resolver)).unwrap();
    db.cite(&id, Some("task/2"), Some(&resolver)).unwrap();

    let mut lessons = db
        .query_for_context(&symbol_ctx("stable_fn", None))
        .unwrap()
        .lessons;
    db.apply_staleness(&mut lessons, &resolver).unwrap();
    assert_eq!(lessons[0].stale, Some(false));
}

#[test]
fn unverified_has_no_stale_flag() {
    let (_dir, db) = setup_lessons_db();
    db.store(&StoreLessonParams {
        text: "Unverified lesson",
        anchors: &[(AnchorKind::Symbol, "some_fn")],
        categories: &[],
        source_task_ids: &[],
        project_origin: None,
    })
    .unwrap();

    let resolver = |_kind: &str, _value: &str| -> Option<String> { Some("any".to_string()) };

    let mut lessons = db
        .query_for_context(&symbol_ctx("some_fn", None))
        .unwrap()
        .lessons;
    assert!(!lessons[0].verified);
    db.apply_staleness(&mut lessons, &resolver).unwrap();
    assert_eq!(lessons[0].stale, None, "unverified lessons keep stale=None");
}

#[test]
fn multi_anchor_one_changed_is_stale() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Multi-anchor staleness",
            anchors: &[
                (AnchorKind::Symbol, "fn_a"),
                (AnchorKind::File, "src/lib.rs"),
            ],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    let original_resolver = |kind: &str, value: &str| -> Option<String> {
        match (kind, value) {
            ("symbol", "fn_a") => Some("hash_a".to_string()),
            ("file", "src/lib.rs") => Some("hash_b".to_string()),
            _ => None,
        }
    };

    db.cite(&id, Some("task/1"), Some(&original_resolver))
        .unwrap();
    db.cite(&id, Some("task/2"), Some(&original_resolver))
        .unwrap();

    // Only the file anchor changed
    let partial_change_resolver = |kind: &str, value: &str| -> Option<String> {
        match (kind, value) {
            ("symbol", "fn_a") => Some("hash_a".to_string()),
            ("file", "src/lib.rs") => Some("hash_b_changed".to_string()),
            _ => None,
        }
    };

    let mut lessons = db
        .query_for_context(&symbol_ctx("fn_a", None))
        .unwrap()
        .lessons;
    db.apply_staleness(&mut lessons, &partial_change_resolver)
        .unwrap();
    assert_eq!(lessons[0].stale, Some(true), "any anchor change → stale");
}

#[test]
fn cite_without_resolver_still_works() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "No resolver lesson",
            anchors: &[(AnchorKind::Symbol, "no_ws_fn")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    // Cite with None resolver — should still cross threshold, just no snapshots
    let r1 = db.cite(&id, Some("task/1"), None).unwrap();
    assert!(!r1.crossed_threshold);
    let r2 = db.cite(&id, Some("task/2"), None).unwrap();
    assert!(r2.crossed_threshold);

    let conn = db.conn_for_test();
    let av_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM anchor_verification WHERE lesson_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(av_count, 0, "no snapshots without resolver");
}

// ---------------------------------------------------------------------------
// Decay / archive
// ---------------------------------------------------------------------------

fn store_old_lesson(db: &LessonsDb, text: &str, symbol: &str, age_days: i64) -> String {
    let id = db
        .store(&StoreLessonParams {
            text,
            anchors: &[(AnchorKind::Symbol, symbol)],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();
    let conn = db.conn_for_test();
    conn.execute(
        &format!(
            "UPDATE lessons SET created_at = datetime('now', '-{age_days} days') WHERE id = ?1"
        ),
        rusqlite::params![id],
    )
    .unwrap();
    id
}

#[test]
fn archive_decayed_archives_old_unverified() {
    let (_dir, db) = setup_lessons_db();
    let old_id = store_old_lesson(&db, "Old lesson", "old_fn", 100);
    let _fresh_id = store_old_lesson(&db, "Fresh lesson", "fresh_fn", 1);

    let window = 30 * 86400; // 30 days
    let archived = db.archive_decayed(window).unwrap();
    assert_eq!(archived, 1);

    // Old lesson is archived
    let conn = db.conn_for_test();
    let is_archived: bool = conn
        .query_row(
            "SELECT archived FROM lessons WHERE id = ?1",
            rusqlite::params![old_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(is_archived);
}

#[test]
fn archive_decayed_spares_verified() {
    let (_dir, db) = setup_lessons_db();
    let id = store_old_lesson(&db, "Verified old lesson", "verified_fn", 100);

    // Verify it
    db.cite(&id, Some("task/1"), None).unwrap();
    db.cite(&id, Some("task/2"), None).unwrap();

    let window = 30 * 86400;
    let archived = db.archive_decayed(window).unwrap();
    assert_eq!(archived, 0, "verified lessons are immune to decay");
}

#[test]
fn archive_decayed_spares_recently_cited() {
    let (_dir, db) = setup_lessons_db();
    let id = store_old_lesson(&db, "Cited lesson", "cited_fn", 100);

    // Cite once — sets last_cited, keeps it alive despite old created_at
    db.cite(&id, Some("task/1"), None).unwrap();

    let window = 30 * 86400;
    let archived = db.archive_decayed(window).unwrap();
    assert_eq!(archived, 0, "cited lessons are immune to decay");
}

#[test]
fn archived_excluded_from_surfacing() {
    let (_dir, db) = setup_lessons_db();
    let id = store_old_lesson(&db, "Soon archived", "archived_fn", 100);

    // Confirm it surfaces before archiving
    let lessons = db
        .query_for_context(&symbol_ctx("archived_fn", None))
        .unwrap()
        .lessons;
    assert_eq!(lessons.len(), 1);

    // Backdate last_surfaced so it's outside the decay window
    {
        let conn = db.conn_for_test();
        conn.execute(
            "UPDATE lessons SET last_surfaced = datetime('now', '-100 days') WHERE id = ?1",
            rusqlite::params![id],
        )
        .unwrap();
    }

    db.archive_decayed(30 * 86400).unwrap();

    let lessons = db
        .query_for_context(&symbol_ctx("archived_fn", None))
        .unwrap()
        .lessons;
    assert_eq!(lessons.len(), 0, "archived lessons stop surfacing");

    // But still findable via search with include_archived
    let results = db
        .search(&LessonsSearchParams {
            include_archived: true,
            ..search_params()
        })
        .unwrap();
    assert!(
        results.iter().any(|l| l.id == id),
        "archived lessons visible with include_archived"
    );
}

#[test]
fn archive_decayed_spares_recently_surfaced() {
    let (_dir, db) = setup_lessons_db();
    let id = store_old_lesson(&db, "Surfaced lesson", "surfaced_fn", 100);

    // Surface it (query_for_context sets last_surfaced)
    let _ = db
        .query_for_context(&symbol_ctx("surfaced_fn", None))
        .unwrap();

    let window = 30 * 86400;
    let archived = db.archive_decayed(window).unwrap();
    assert_eq!(archived, 0, "recently surfaced lessons are immune");

    // Now backdate last_surfaced too
    let conn = db.conn_for_test();
    conn.execute(
        "UPDATE lessons SET last_surfaced = datetime('now', '-100 days') WHERE id = ?1",
        rusqlite::params![id],
    )
    .unwrap();
    drop(conn);

    let archived = db.archive_decayed(window).unwrap();
    assert_eq!(archived, 1, "old-surfaced lessons get archived");
}

#[test]
fn archive_decayed_archives_old_cited_unverified() {
    let (_dir, db) = setup_lessons_db();
    let id = store_old_lesson(&db, "Old cited lesson", "old_cited_fn", 100);

    // Cite once (doesn't reach verification threshold)
    db.cite(&id, Some("task/1"), None).unwrap();

    // Backdate last_cited to outside the window
    let conn = db.conn_for_test();
    conn.execute(
        "UPDATE lessons SET last_cited = datetime('now', '-100 days') WHERE id = ?1",
        rusqlite::params![id],
    )
    .unwrap();
    drop(conn);

    let window = 30 * 86400;
    let archived = db.archive_decayed(window).unwrap();
    assert_eq!(archived, 1, "old-cited unverified lessons should decay");
}

#[test]
fn verified_without_snapshot_has_no_stale_flag() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Pre-migration verified lesson",
            anchors: &[(AnchorKind::Symbol, "legacy_fn")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    // Verify without a resolver (simulates pre-migration or no workspace)
    db.cite(&id, Some("task/1"), None).unwrap();
    db.cite(&id, Some("task/2"), None).unwrap();

    let resolver = |_kind: &str, _value: &str| -> Option<String> { Some("any_hash".to_string()) };

    let mut lessons = db
        .query_for_context(&symbol_ctx("legacy_fn", None))
        .unwrap()
        .lessons;
    assert!(lessons[0].verified);

    db.apply_staleness(&mut lessons, &resolver).unwrap();
    assert_eq!(
        lessons[0].stale, None,
        "verified without snapshot should be None, not Some(false)"
    );
}

// ---------------------------------------------------------------------------
// Bug reproductions (sutra/162 review)
// ---------------------------------------------------------------------------

#[test]
fn anti_verify_twice_same_lesson() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "lesson",
            anchors: &[],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    db.cite(&id, Some("task/1"), None).unwrap();
    db.cite(&id, Some("task/2"), None).unwrap();
    db.cite(&id, Some("task/3"), None).unwrap();
    // confidence = 3, verified

    let r1 = db.anti_verify(&id).unwrap();
    assert_eq!(r1.new_confidence, 2);
    assert!(r1.verified); // still at threshold

    // Second anti-verify should also succeed — different negative signal
    let r2 = db.anti_verify(&id);
    assert!(
        r2.is_ok(),
        "second anti_verify should not fail: {:?}",
        r2.err()
    );
    assert_eq!(r2.unwrap().new_confidence, 1);
}

#[test]
fn stale_when_anchor_deleted() {
    let (_dir, db) = setup_lessons_db();
    let id = db
        .store(&StoreLessonParams {
            text: "Lesson about deleted fn",
            anchors: &[(AnchorKind::Symbol, "deleted_fn")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

    let resolver = |kind: &str, value: &str| -> Option<String> {
        if kind == "symbol" && value == "deleted_fn" {
            Some("original_hash".to_string())
        } else {
            None
        }
    };

    db.cite(&id, Some("task/1"), Some(&resolver)).unwrap();
    db.cite(&id, Some("task/2"), Some(&resolver)).unwrap();
    // Now verified with snapshot

    // Symbol deleted — resolver returns None
    let gone_resolver = |_kind: &str, _value: &str| -> Option<String> { None };

    let mut lessons = db
        .query_for_context(&symbol_ctx("deleted_fn", None))
        .unwrap()
        .lessons;
    assert_eq!(lessons.len(), 1);

    db.apply_staleness(&mut lessons, &gone_resolver).unwrap();
    assert_eq!(
        lessons[0].stale,
        Some(true),
        "deleted anchor should be marked stale, not treated as current"
    );
}
