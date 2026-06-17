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
        .unwrap();
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
        .unwrap();
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

    let lessons = db.query_for_context(&symbol_ctx("some_fn", None)).unwrap();
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
            .len(),
        1
    );
    assert_eq!(
        db.query_for_context(&symbol_ctx("fn_b", None))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        db.query_for_context(&symbol_ctx("fn_c", None))
            .unwrap()
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
        .unwrap();

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
        .unwrap();
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
        .unwrap();
    assert_eq!(chitta_lessons.len(), 2);
    assert!(
        chitta_lessons
            .iter()
            .any(|l| l.text.contains("chitta-specific"))
    );
    assert!(chitta_lessons.iter().any(|l| l.text.contains("global")));

    let all_lessons = db.query_for_context(&symbol_ctx("init", None)).unwrap();
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
        })
        .unwrap();
    assert_eq!(hit.len(), 1);
    assert!(hit[0].text.contains("DB layer"));

    let miss = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: Some("src/tools/read.rs"),
            imports: &[],
            project: None,
        })
        .unwrap();
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
        })
        .unwrap();
    assert_eq!(hit.len(), 1);
    assert!(hit[0].text.contains("rusqlite pitfall"));

    let imports_miss = vec!["serde_json::json".to_string()];
    let miss = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: None,
            imports: &imports_miss,
            project: None,
        })
        .unwrap();
    assert!(miss.is_empty());
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
        })
        .unwrap();
    assert_eq!(hit.len(), 1);

    let miss_other = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: Some("src/tools/read.rs"),
            imports: &[],
            project: None,
        })
        .unwrap();
    assert!(miss_other.is_empty());

    // "src/dba/foo.rs" must NOT match "src/db" — no false prefix
    let miss_prefix = db
        .query_for_context(&MatchContext {
            symbol_name: "irrelevant",
            file_path: Some("src/dba/foo.rs"),
            imports: &[],
            project: None,
        })
        .unwrap();
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
        })
        .unwrap();
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
        })
        .unwrap();
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
        .unwrap();
    assert_eq!(hit.len(), 1);
    assert!(hit[0].text.contains("Short name"));

    // Direct short name still works
    let direct = db
        .query_for_context(&symbol_ctx("query_for_context", None))
        .unwrap();
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
        })
        .unwrap();
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
        })
        .unwrap();
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
