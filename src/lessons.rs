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
    (
        "0003_cite_idempotency",
        include_str!("lessons_cite_idempotency.sql"),
    ),
    ("0004_staleness", include_str!("lessons_staleness.sql")),
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

const CONTEXT_SURFACING_CAP: usize = 10;

#[derive(Debug, Clone, Serialize)]
pub struct ContextLessons {
    pub lessons: Vec<SurfacedLesson>,
    pub omitted: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct SurfacedLesson {
    pub id: String,
    pub text: String,
    pub verified: bool,
    pub confidence: i64,
    pub project_origin: Option<String>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale: Option<bool>,
}

const GLOB_OPTS: glob::MatchOptions = glob::MatchOptions {
    case_sensitive: true,
    require_literal_separator: true,
    require_literal_leading_dot: false,
};

fn matches_anchor(kind: &str, value: &str, ctx: &MatchContext<'_>) -> bool {
    match kind {
        "file" => {
            let Some(fp) = ctx.file_path else {
                return false;
            };
            glob::Pattern::new(value)
                .map(|p| p.matches_with(fp, GLOB_OPTS))
                .unwrap_or(false)
        }
        "import_pattern" => {
            let Ok(pat) = glob::Pattern::new(value) else {
                return false;
            };
            ctx.imports
                .iter()
                .any(|imp| pat.matches_with(imp, GLOB_OPTS))
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
        stale: None,
    })
}

impl LessonsDb {
    pub fn query_for_context(&self, ctx: &MatchContext<'_>) -> Result<ContextLessons> {
        let conn = self.conn.lock();

        let mut seen = HashSet::new();
        let mut lessons = Vec::new();
        // Track which anchor keys actually caused each lesson to surface
        let mut matched_anchors: HashMap<String, HashSet<String>> = HashMap::new();

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
                        l.project_origin, l.created_at, a.value
                 FROM lessons l
                 JOIN anchors a ON a.lesson_id = l.id
                 WHERE a.kind = 'symbol' AND (a.value = ?1 OR (?3 AND a.value = ?2))
                   AND l.archived = 0
                   AND (l.project_origin IS NULL OR l.project_origin = ?4 OR ?4 IS NULL)
                 ORDER BY l.verified DESC, l.confidence DESC",
            )?;
            let mut rows = stmt.query(params![
                ctx.symbol_name,
                short_name,
                has_qualifier,
                ctx.project
            ])?;
            while let Some(row) = rows.next()? {
                let id: String = row.get(0)?;
                let anchor_val: String = row.get(6)?;
                let key = format!("symbol:{anchor_val}");
                matched_anchors.entry(id.clone()).or_default().insert(key);
                if !seen.contains(&id) {
                    seen.insert(id);
                    lessons.push(map_surfaced_lesson(row)?);
                }
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
                let anchor_kind: String = row.get(6)?;
                let anchor_value: String = row.get(7)?;
                if matches_anchor(&anchor_kind, &anchor_value, ctx) {
                    let key = format!("{anchor_kind}:{anchor_value}");
                    matched_anchors.entry(id.clone()).or_default().insert(key);
                    if !seen.contains(&id) {
                        seen.insert(id);
                        lessons.push(map_surfaced_lesson(row)?);
                    }
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
                let lang_tags: Vec<&str> = cats
                    .iter()
                    .filter(|c| is_language_category(c))
                    .map(|c| c.as_str())
                    .collect();
                if lang_tags.is_empty() {
                    return true;
                }
                lang_tags.iter().any(|t| ws_lang_set.contains(t))
            });
        }

        // Phase 4: verified-first surfacing priority — suppress unverified
        // lessons when a verified lesson matched the same anchor in this context.
        if lessons.iter().any(|l| l.verified) && lessons.iter().any(|l| !l.verified) {
            let verified_matched: HashSet<&str> = lessons
                .iter()
                .filter(|l| l.verified)
                .flat_map(|l| {
                    matched_anchors
                        .get(&l.id)
                        .into_iter()
                        .flatten()
                        .map(|s| s.as_str())
                })
                .collect();

            lessons.retain(|l| {
                if l.verified {
                    return true;
                }
                let dominated = matched_anchors
                    .get(&l.id)
                    .map(|keys| keys.iter().any(|k| verified_matched.contains(k.as_str())))
                    .unwrap_or(false);
                !dominated
            });
        }

        // Tag all surviving unverified lessons
        for l in &mut lessons {
            if !l.verified {
                l.text = format!("[unverified] {}", l.text);
            }
        }

        // Sort verified-first before applying cap so verified lessons always
        // take priority slots.
        lessons.sort_by(|a, b| {
            b.verified
                .cmp(&a.verified)
                .then(b.confidence.cmp(&a.confidence))
        });

        let total = lessons.len();
        let omitted = total.saturating_sub(CONTEXT_SURFACING_CAP);
        lessons.truncate(CONTEXT_SURFACING_CAP);

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

        Ok(ContextLessons { lessons, omitted })
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

fn quote_fts5_query(raw: &str) -> String {
    raw.split_whitespace()
        .map(|term| {
            let escaped = term.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct LessonsSearchParams<'a> {
    pub query: Option<&'a str>,
    pub category: Option<&'a str>,
    pub symbol: Option<&'a str>,
    pub verified: Option<bool>,
    pub project: Option<&'a str>,
    pub include_archived: bool,
    pub limit: usize,
}

impl LessonsDb {
    pub fn search(&self, params: &LessonsSearchParams<'_>) -> Result<Vec<SurfacedLesson>> {
        let conn = self.conn.lock();

        let mut sql = String::from(
            "SELECT DISTINCT l.id, l.text, l.verified, l.confidence, \
             l.project_origin, l.created_at FROM lessons l",
        );
        let mut conditions: Vec<String> = if params.include_archived {
            vec![]
        } else {
            vec!["l.archived = 0".to_string()]
        };
        let mut bind_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut param_idx = 0usize;

        let has_fts = params.query.is_some();

        if let Some(q) = params.query {
            sql.push_str(" JOIN lessons_fts ON lessons_fts.rowid = l.rowid");
            param_idx += 1;
            conditions.push(format!("lessons_fts MATCH ?{param_idx}"));
            let safe_q = quote_fts5_query(q);
            bind_values.push(Box::new(safe_q));
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

        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }

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
// Citation / verification lifecycle
// ---------------------------------------------------------------------------

const VERIFICATION_THRESHOLD: i64 = 2;

pub struct CiteResult {
    pub new_confidence: i64,
    pub verified: bool,
    pub crossed_threshold: bool,
}

pub struct AntiVerifyResult {
    pub new_confidence: i64,
    pub verified: bool,
}

/// Resolves an anchor (kind, value) to the current content hash of its backing file.
/// Returns `None` for anchor kinds that aren't hashable (directory, import_pattern)
/// or when the symbol/file can't be resolved.
pub type HashResolver<'a> = dyn Fn(&str, &str) -> Option<String> + 'a;

impl LessonsDb {
    pub fn cite(
        &self,
        lesson_id: &str,
        task_id: Option<&str>,
        hash_resolver: Option<&HashResolver<'_>>,
    ) -> Result<CiteResult> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        let (old_confidence, old_verified): (i64, bool) = tx
            .query_row(
                "SELECT confidence, verified FROM lessons WHERE id = ?1 AND archived = 0",
                params![lesson_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| {
                SutraError::Internal(format!("lesson not found or archived: {lesson_id}"))
            })?;

        tx.execute(
            "INSERT OR IGNORE INTO citations (lesson_id, task_id, field) VALUES (?1, ?2, 'cite')",
            params![lesson_id, task_id.unwrap_or("")],
        )?;
        let inserted = tx.changes() > 0;

        let new_confidence = if inserted {
            old_confidence + 1
        } else {
            old_confidence
        };
        let now_verified = new_confidence >= VERIFICATION_THRESHOLD;
        let crossed = now_verified && !old_verified;

        if inserted {
            let verified_at_clause = if crossed {
                ", verified_at = datetime('now')"
            } else {
                ""
            };
            tx.execute(
                &format!(
                    "UPDATE lessons SET confidence = ?1, verified = ?2, last_cited = datetime('now'){verified_at_clause} \
                     WHERE id = ?3"
                ),
                params![new_confidence, now_verified, lesson_id],
            )?;
        }

        if crossed {
            Self::snapshot_anchor_hashes(&tx, lesson_id, hash_resolver)?;
        }

        tx.commit()?;
        Ok(CiteResult {
            new_confidence,
            verified: now_verified,
            crossed_threshold: crossed,
        })
    }

    pub fn anti_verify(&self, lesson_id: &str) -> Result<AntiVerifyResult> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        let old_confidence: i64 = tx
            .query_row(
                "SELECT confidence FROM lessons WHERE id = ?1 AND archived = 0",
                params![lesson_id],
                |row| row.get(0),
            )
            .map_err(|_| {
                SutraError::Internal(format!("lesson not found or archived: {lesson_id}"))
            })?;

        tx.execute(
            "INSERT INTO citations (lesson_id, task_id, field) VALUES (?1, '', 'anti_verify')",
            params![lesson_id],
        )?;

        let new_confidence = (old_confidence - 1).max(0);
        let still_verified = new_confidence >= VERIFICATION_THRESHOLD;

        tx.execute(
            "UPDATE lessons SET confidence = ?1, verified = ?2 WHERE id = ?3",
            params![new_confidence, still_verified, lesson_id],
        )?;

        tx.commit()?;
        Ok(AntiVerifyResult {
            new_confidence,
            verified: still_verified,
        })
    }
}

// ---------------------------------------------------------------------------
// Decay / archive
// ---------------------------------------------------------------------------

impl LessonsDb {
    /// Archive unverified lessons that haven't been cited or surfaced within
    /// `window_secs` seconds. Returns the number of lessons archived.
    pub fn archive_decayed(&self, window_secs: i64) -> Result<usize> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE lessons SET archived = 1
             WHERE archived = 0
               AND verified = 0
               AND last_cited IS NULL
               AND (last_surfaced IS NULL OR last_surfaced < datetime('now', ?1))
               AND created_at < datetime('now', ?1)",
            params![format!("-{window_secs} seconds")],
        )?;
        Ok(changed)
    }
}

// ---------------------------------------------------------------------------
// Staleness detection
// ---------------------------------------------------------------------------

impl LessonsDb {
    fn snapshot_anchor_hashes(
        tx: &rusqlite::Transaction<'_>,
        lesson_id: &str,
        hash_resolver: Option<&HashResolver<'_>>,
    ) -> Result<()> {
        let resolver = match hash_resolver {
            Some(r) => r,
            None => return Ok(()),
        };

        let mut stmt = tx.prepare("SELECT id, kind, value FROM anchors WHERE lesson_id = ?1")?;
        let anchors: Vec<(i64, String, String)> = stmt
            .query_map(params![lesson_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);

        for (anchor_id, kind, value) in &anchors {
            if let Some(hash) = resolver(kind, value) {
                tx.execute(
                    "INSERT INTO anchor_verification (lesson_id, anchor_id, content_hash, verified_at)
                     VALUES (?1, ?2, ?3, datetime('now'))
                     ON CONFLICT(anchor_id) DO UPDATE SET content_hash = ?3, verified_at = datetime('now')",
                    params![lesson_id, anchor_id, hash],
                )?;
            }
        }
        Ok(())
    }

    /// For each surfaced lesson, check whether any verified anchor's content
    /// has changed since verification. Returns a map of lesson_id → stale.
    /// Only lessons with `anchor_verification` rows are checked.
    pub fn check_staleness(
        &self,
        lesson_ids: &[&str],
        hash_resolver: &HashResolver<'_>,
    ) -> Result<HashMap<String, bool>> {
        if lesson_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let conn = self.conn.lock();
        let placeholders = lesson_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT av.lesson_id, a.kind, a.value, av.content_hash
             FROM anchor_verification av
             JOIN anchors a ON a.id = av.anchor_id
             WHERE av.lesson_id IN ({placeholders})"
        ))?;
        let rows: Vec<(String, String, String, String)> = stmt
            .query_map(rusqlite::params_from_iter(lesson_ids.iter()), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let mut result: HashMap<String, bool> = HashMap::new();
        for (lesson_id, kind, value, snapshot_hash) in &rows {
            let stale_entry = result.entry(lesson_id.clone()).or_insert(false);
            if *stale_entry {
                continue;
            }
            match hash_resolver(kind, value) {
                Some(current_hash) if current_hash != *snapshot_hash => {
                    *stale_entry = true;
                }
                _ => {}
            }
        }
        Ok(result)
    }

    /// Annotate surfaced lessons with staleness flags in place.
    /// Verified lessons with anchor_verification snapshots get `Some(true/false)`;
    /// unverified lessons stay `None`.
    pub fn apply_staleness(
        &self,
        lessons: &mut [SurfacedLesson],
        hash_resolver: &HashResolver<'_>,
    ) -> Result<()> {
        let verified_ids: Vec<&str> = lessons
            .iter()
            .filter(|l| l.verified)
            .map(|l| l.id.as_str())
            .collect();
        if verified_ids.is_empty() {
            return Ok(());
        }
        let stale_map = self.check_staleness(&verified_ids, hash_resolver)?;
        for lesson in lessons.iter_mut() {
            if lesson.verified {
                lesson.stale = Some(stale_map.get(&lesson.id).copied().unwrap_or(false));
            }
        }
        Ok(())
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
