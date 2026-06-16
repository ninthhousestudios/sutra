use std::path::Path;

use parking_lot::Mutex;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::error::{Result, SutraError};

// ---------------------------------------------------------------------------
// LessonsDb
// ---------------------------------------------------------------------------

pub struct LessonsDb {
    conn: Mutex<Connection>,
}

const MIGRATIONS: &[(&str, &str)] = &[("0001_initial", include_str!("lessons_schema.sql"))];

impl LessonsDb {
    pub fn open(db_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(db_dir).map_err(|e| {
            SutraError::Internal(format!("cannot create {}: {e}", db_dir.display()))
        })?;
        let db_path = db_dir.join("lessons.db");
        let conn = Connection::open(&db_path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA synchronous = NORMAL;\
             PRAGMA foreign_keys = ON;\
             PRAGMA busy_timeout = 5000;",
        )?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.run_migrations()?;
        Ok(db)
    }

    fn run_migrations(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 name         TEXT    NOT NULL UNIQUE,
                 content_hash TEXT    NOT NULL,
                 applied_at   TEXT    NOT NULL
             )",
        )?;

        for &(name, sql) in MIGRATIONS {
            let hash = blake3::hash(sql.as_bytes()).to_hex().to_string();

            let existing: Option<String> = conn
                .query_row(
                    "SELECT content_hash FROM schema_migrations WHERE name = ?1",
                    params![name],
                    |row| row.get(0),
                )
                .ok();

            if let Some(stored) = existing {
                if stored != hash {
                    return Err(SutraError::Internal(format!(
                        "lessons migration `{name}` hash mismatch: stored={stored}, current={hash}"
                    )));
                }
                continue;
            }

            let sp = format!("migration_{name}");
            conn.execute_batch(&format!("SAVEPOINT {sp}"))?;

            match conn.execute_batch(sql) {
                Ok(()) => {
                    conn.execute(
                        "INSERT INTO schema_migrations (name, content_hash, applied_at) \
                         VALUES (?1, ?2, datetime('now'))",
                        params![name, hash],
                    )?;
                    conn.execute_batch(&format!("RELEASE SAVEPOINT {sp}"))?;
                }
                Err(e) => {
                    let _ = conn.execute_batch(&format!("ROLLBACK TO SAVEPOINT {sp}"));
                    let _ = conn.execute_batch(&format!("RELEASE SAVEPOINT {sp}"));
                    return Err(SutraError::Internal(format!(
                        "lessons migration `{name}` failed: {e}"
                    )));
                }
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Anchor kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    Symbol,
    File,
    ImportPattern,
    Directory,
}

impl AnchorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::File => "file",
            Self::ImportPattern => "import_pattern",
            Self::Directory => "directory",
        }
    }
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

pub struct StoreLessonParams<'a> {
    pub text: &'a str,
    pub anchors: &'a [(AnchorKind, &'a str)],
    pub categories: &'a [&'a str],
    pub source_task_ids: &'a [&'a str],
    pub project_origin: Option<&'a str>,
}

impl LessonsDb {
    pub fn store(&self, params: &StoreLessonParams<'_>) -> Result<String> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let id = uuid::Uuid::now_v7().to_string();

        tx.execute(
            "INSERT INTO lessons (id, text, project_origin) VALUES (?1, ?2, ?3)",
            params![id, params.text, params.project_origin],
        )?;

        for &(kind, value) in params.anchors {
            tx.execute(
                "INSERT INTO anchors (lesson_id, kind, value) VALUES (?1, ?2, ?3)",
                params![id, kind.as_str(), value],
            )?;
        }

        for tag in params.categories {
            tx.execute(
                "INSERT OR IGNORE INTO categories (lesson_id, tag) VALUES (?1, ?2)",
                params![id, tag],
            )?;
        }

        for task_id in params.source_task_ids {
            tx.execute(
                "INSERT INTO citations (lesson_id, task_id, field) VALUES (?1, ?2, 'source')",
                params![id, task_id],
            )?;
        }

        tx.commit()?;
        Ok(id)
    }
}

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct SurfacedLesson {
    pub id: String,
    pub text: String,
    pub verified: bool,
    pub confidence: i64,
    pub project_origin: Option<String>,
    pub created_at: String,
}

impl LessonsDb {
    pub fn query_for_context(
        &self,
        symbol_name: &str,
        project: Option<&str>,
    ) -> Result<Vec<SurfacedLesson>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT l.id, l.text, l.verified, l.confidence,
                    l.project_origin, l.created_at
             FROM lessons l
             JOIN anchors a ON a.lesson_id = l.id
             WHERE a.kind = 'symbol' AND a.value = ?1
               AND l.archived = 0
               AND (l.project_origin IS NULL OR l.project_origin = ?2 OR ?2 IS NULL)
             ORDER BY l.verified DESC, l.confidence DESC",
        )?;
        let lessons: Vec<SurfacedLesson> = stmt
            .query_map(params![symbol_name, project], |row| {
                Ok(SurfacedLesson {
                    id: row.get(0)?,
                    text: row.get(1)?,
                    verified: row.get::<_, i64>(2)? != 0,
                    confidence: row.get(3)?,
                    project_origin: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if !lessons.is_empty() {
            let ids: Vec<&str> = lessons.iter().map(|l| l.id.as_str()).collect();
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            conn.execute(
                &format!(
                    "UPDATE lessons SET last_surfaced = datetime('now') WHERE id IN ({placeholders})"
                ),
                rusqlite::params_from_iter(ids.iter()),
            )?;
        }

        Ok(lessons)
    }
}

// ---------------------------------------------------------------------------
// Test support
// ---------------------------------------------------------------------------

impl LessonsDb {
    #[doc(hidden)]
    pub fn conn_for_test(&self) -> parking_lot::MutexGuard<'_, Connection> {
        self.conn.lock()
    }
}
