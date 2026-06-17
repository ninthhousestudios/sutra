use std::collections::{HashMap, HashSet};
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

const MIGRATIONS: &[(&str, &str)] = &[
    ("0001_initial", include_str!("lessons_schema.sql")),
    ("0002_fts5", include_str!("lessons_fts5.sql")),
];

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

const KNOWN_LANGUAGE_CATEGORIES: &[&str] = &["rust", "dart"];

fn is_language_category(cat: &str) -> bool {
    KNOWN_LANGUAGE_CATEGORIES
        .iter()
        .any(|&lc| lc.eq_ignore_ascii_case(cat))
}

pub struct MatchContext<'a> {
    pub symbol_name: &'a str,
    pub file_path: Option<&'a str>,
    pub imports: &'a [String],
    pub project: Option<&'a str>,
    pub workspace_languages: &'a [String],
}

#[derive(Debug, Clone, Serialize)]
pub struct SurfacedLesson {
    pub id: String,
    pub text: String,
    pub verified: bool,
    pub confidence: i64,
    pub project_origin: Option<String>,
    pub created_at: String,
}

fn matches_anchor(kind: &str, value: &str, ctx: &MatchContext<'_>) -> bool {
    match kind {
        "file" => {
            let Some(fp) = ctx.file_path else {
                return false;
            };
            glob::Pattern::new(value)
                .map(|p| p.matches(fp))
                .unwrap_or(false)
        }
        "import_pattern" => {
            let Ok(pat) = glob::Pattern::new(value) else {
                return false;
            };
            ctx.imports.iter().any(|imp| pat.matches(imp))
        }
        "directory" => {
            let Some(fp) = ctx.file_path else {
                return false;
            };
            let dir = value.trim_end_matches('/');
            fp.starts_with(dir) && fp.as_bytes().get(dir.len()) == Some(&b'/')
        }
        _ => false,
    }
}

fn map_surfaced_lesson(row: &rusqlite::Row<'_>) -> rusqlite::Result<SurfacedLesson> {
    Ok(SurfacedLesson {
        id: row.get(0)?,
        text: row.get(1)?,
        verified: row.get::<_, i64>(2)? != 0,
        confidence: row.get(3)?,
        project_origin: row.get(4)?,
        created_at: row.get(5)?,
    })
}

impl LessonsDb {
    pub fn query_for_context(&self, ctx: &MatchContext<'_>) -> Result<Vec<SurfacedLesson>> {
        let conn = self.conn.lock();

        let mut seen = HashSet::new();
        let mut lessons = Vec::new();

        // Phase 1: symbol match (indexed, fast). Also checks short name
        // so anchors stored as "foo" match when the caller passes "Mod::foo".
        let short_name = ctx
            .symbol_name
            .rsplit("::")
            .next()
            .unwrap_or(ctx.symbol_name);
        let has_qualifier = short_name != ctx.symbol_name;
        {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT l.id, l.text, l.verified, l.confidence,
                        l.project_origin, l.created_at
                 FROM lessons l
                 JOIN anchors a ON a.lesson_id = l.id
                 WHERE a.kind = 'symbol' AND (a.value = ?1 OR (?3 AND a.value = ?2))
                   AND l.archived = 0
                   AND (l.project_origin IS NULL OR l.project_origin = ?4 OR ?4 IS NULL)
                 ORDER BY l.verified DESC, l.confidence DESC",
            )?;
            let rows = stmt
                .query_map(
                    params![ctx.symbol_name, short_name, has_qualifier, ctx.project],
                    map_surfaced_lesson,
                )?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            for lesson in rows {
                seen.insert(lesson.id.clone());
                lessons.push(lesson);
            }
        }

        // Phase 2: file/import/directory anchors — load candidates, filter in Rust
        if ctx.file_path.is_some() || !ctx.imports.is_empty() {
            let mut stmt = conn.prepare(
                "SELECT l.id, l.text, l.verified, l.confidence,
                        l.project_origin, l.created_at, a.kind, a.value
                 FROM lessons l
                 JOIN anchors a ON a.lesson_id = l.id
                 WHERE a.kind IN ('file', 'import_pattern', 'directory')
                   AND l.archived = 0
                   AND (l.project_origin IS NULL OR l.project_origin = ?1 OR ?1 IS NULL)",
            )?;
            let mut rows = stmt.query(params![ctx.project])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                if seen.contains(&id) {
                    continue;
                }
                let anchor_kind: String = row.get(6)?;
                let anchor_value: String = row.get(7)?;
                if matches_anchor(&anchor_kind, &anchor_value, ctx) {
                    seen.insert(id);
                    lessons.push(map_surfaced_lesson(row)?);
                }
            }
        }

        // Phase 3: category filtering — exclude language-specific lessons
        // irrelevant to this workspace. Skip when workspace_languages is empty
        // (no workspace context → surface everything).
        if !lessons.is_empty() && !ctx.workspace_languages.is_empty() {
            let ids: Vec<&str> = lessons.iter().map(|l| l.id.as_str()).collect();
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT lesson_id, tag FROM categories WHERE lesson_id IN ({placeholders})"
            ))?;
            let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
            let mut cat_map: HashMap<String, Vec<String>> = HashMap::new();
            while let Some(row) = rows.next()? {
                let lid: String = row.get(0)?;
                let tag: String = row.get(1)?;
                cat_map.entry(lid).or_default().push(tag);
            }
            drop(rows);
            drop(stmt);

            let ws_lang_set: HashSet<&str> =
                ctx.workspace_languages.iter().map(|s| s.as_str()).collect();

            lessons.retain(|l| {
                let Some(cats) = cat_map.get(&l.id) else {
                    return true;
                };
                if cats.is_empty() {
                    return true;
                }
                cats.iter().any(|c| {
                    if is_language_category(c) {
                        ws_lang_set.contains(c.as_str())
                    } else {
                        true
                    }
                })
            });
        }

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
// Search
// ---------------------------------------------------------------------------

pub struct LessonsSearchParams<'a> {
    pub query: Option<&'a str>,
    pub category: Option<&'a str>,
    pub symbol: Option<&'a str>,
    pub verified: Option<bool>,
    pub project: Option<&'a str>,
    pub limit: usize,
}

impl LessonsDb {
    pub fn search(&self, params: &LessonsSearchParams<'_>) -> Result<Vec<SurfacedLesson>> {
        let conn = self.conn.lock();

        let mut sql = String::from(
            "SELECT DISTINCT l.id, l.text, l.verified, l.confidence, \
             l.project_origin, l.created_at FROM lessons l",
        );
        let mut conditions: Vec<String> = vec!["l.archived = 0".to_string()];
        let mut bind_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 0usize;

        let has_fts = params.query.is_some();

        if let Some(q) = params.query {
            sql.push_str(" JOIN lessons_fts ON lessons_fts.rowid = l.rowid");
            param_idx += 1;
            conditions.push(format!("lessons_fts MATCH ?{param_idx}"));
            bind_values.push(Box::new(q.to_string()));
        }

        if let Some(cat) = params.category {
            sql.push_str(" JOIN categories c ON c.lesson_id = l.id");
            param_idx += 1;
            conditions.push(format!("c.tag = ?{param_idx}"));
            bind_values.push(Box::new(cat.to_string()));
        }

        if let Some(sym) = params.symbol {
            sql.push_str(" JOIN anchors a ON a.lesson_id = l.id");
            param_idx += 1;
            conditions.push(format!("a.kind = 'symbol' AND a.value = ?{param_idx}"));
            bind_values.push(Box::new(sym.to_string()));
        }

        if let Some(true) = params.verified {
            conditions.push("l.verified = 1".to_string());
        }

        if let Some(proj) = params.project {
            param_idx += 1;
            conditions.push(format!(
                "(l.project_origin IS NULL OR l.project_origin = ?{param_idx})"
            ));
            bind_values.push(Box::new(proj.to_string()));
        }

        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));

        if has_fts {
            sql.push_str(" ORDER BY rank");
        } else {
            sql.push_str(" ORDER BY l.verified DESC, l.confidence DESC");
        }

        param_idx += 1;
        sql.push_str(&format!(" LIMIT ?{param_idx}"));
        bind_values.push(Box::new(params.limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::types::ToSql> =
            bind_values.iter().map(|b| b.as_ref()).collect();
        let lessons: Vec<SurfacedLesson> = stmt
            .query_map(refs.as_slice(), map_surfaced_lesson)?
            .collect::<std::result::Result<Vec<_>, _>>()?;

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
