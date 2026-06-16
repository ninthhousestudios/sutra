use sutra::lessons::{AnchorKind, LessonsDb, StoreLessonParams};
use tempfile::tempdir;

fn setup_lessons_db() -> (tempfile::TempDir, LessonsDb) {
    let dir = tempdir().unwrap();
    let db = LessonsDb::open(dir.path()).unwrap();
    (dir, db)
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

    let lessons = db.query_for_context("refresh_index").unwrap();
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

    let lessons = db.query_for_context("completely_different").unwrap();
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

    let lessons = db.query_for_context("some_fn").unwrap();
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

    assert_eq!(db.query_for_context("fn_a").unwrap().len(), 1);
    assert_eq!(db.query_for_context("fn_b").unwrap().len(), 1);
    assert_eq!(db.query_for_context("fn_c").unwrap().len(), 0);
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

    // Before query, last_surfaced should be NULL
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

    let _ = db.query_for_context("target_fn").unwrap();

    // After query, last_surfaced should be set
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
