//! SQLite database layer for sutra.
//!
//! All timestamps are stored as ISO-8601 strings (TIMESTAMP columns). Access
//! is serialised through a `parking_lot::Mutex<Connection>` — single-writer
//! model, correct for one daemon with short-lived transactions.

use std::path::Path;

use parking_lot::Mutex;
use rusqlite::{Connection, params};

use crate::error::{Result, SutraError};

// ---------------------------------------------------------------------------
// Row types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct FileRow {
    pub id: i64,
    pub path: String,
    pub language: String,
    pub content_hash: String,
    pub line_count: i64,
    pub parsed_ok: bool,
    pub last_parsed: String,
    pub fan_in_files: i64,
    pub blast_radius: i64,
    pub pagerank: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct SymbolRow {
    pub id: i64,
    pub file_id: i64,
    pub qualified_name: String,
    pub short_name: String,
    pub kind: String,
    pub signature: Option<String>,
    pub signature_hash: Option<String>,
    pub visibility: Option<String>,
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: i64,
    pub end_col: i64,
    pub parent_symbol_id: Option<i64>,
    pub docstring: Option<String>,
    pub pagerank: Option<f64>,
    pub cyclomatic: Option<i64>,
    pub cognitive: Option<i64>,
    pub flags: i64,
}

pub struct InsertSymbolParams<'a> {
    pub file_id: i64,
    pub qualified_name: &'a str,
    pub short_name: &'a str,
    pub kind: &'a str,
    pub signature: Option<&'a str>,
    pub signature_hash: Option<&'a str>,
    pub visibility: Option<&'a str>,
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: i64,
    pub end_col: i64,
    pub parent_symbol_id: Option<i64>,
    pub docstring: Option<&'a str>,
    pub cyclomatic: Option<i64>,
    pub cognitive: Option<i64>,
    pub flags: i64,
}

#[derive(Debug, Clone)]
pub struct RefRow {
    pub id: i64,
    pub file_id: i64,
    pub target_symbol_id: Option<i64>,
    pub unresolved_name: Option<String>,
    pub line: i64,
    pub col: i64,
    pub context_kind: String,
}

#[derive(Debug, Clone)]
pub struct ImportRow {
    pub id: i64,
    pub file_id: i64,
    pub imported_path: String,
    pub resolved_file_id: Option<i64>,
    pub line: i64,
}

#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub id: i64,
    pub timestamp: String,
    pub files_parsed: i64,
    pub symbols_extracted: i64,
    pub refs_extracted: i64,
    pub parse_errors: i64,
    pub duration_ms: i64,
    pub total_complexity: i64,
    pub dead_symbol_count: i64,
    pub hotspot_count: i64,
    pub health_score: i64,
}

// ---------------------------------------------------------------------------
// Db
// ---------------------------------------------------------------------------

pub struct Db {
    conn: Mutex<Connection>,
    workspace_id: String,
}

impl Db {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Open the SQLite database at `db_dir/<workspace_id>/index.db`, creating
    /// directories as needed. Applies WAL mode and other PRAGMAs, then runs
    /// the embedded migrations.
    pub fn open(workspace_id: &str, db_dir: &Path) -> Result<Self> {
        let dir = db_dir.join(workspace_id);
        std::fs::create_dir_all(&dir).map_err(|e| {
            SutraError::Internal(format!(
                "could not create database directory {}: {e}",
                dir.display()
            ))
        })?;

        let db_path = dir.join("index.db");
        let conn = Connection::open(&db_path)?;

        // PRAGMAs — WAL first (must precede others on a fresh connection).
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;\
             PRAGMA foreign_keys = ON;\
             PRAGMA busy_timeout = 5000;",
        )?;

        let db = Self {
            conn: Mutex::new(conn),
            workspace_id: workspace_id.to_string(),
        };
        db.run_migrations()?;
        Ok(db)
    }

    /// Execute the embedded migration SQL.
    fn run_migrations(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(include_str!("../migrations/0001_initial.sql"))?;
        // Idempotent: ALTER TABLE ADD COLUMN fails if column already exists.
        for sql in [
            include_str!("../migrations/0002_complexity.sql"),
            include_str!("../migrations/0003_snapshot_aggregates.sql"),
            include_str!("../migrations/0004_symbol_flags.sql"),
        ] {
            for stmt in sql.lines() {
                let stmt = stmt.trim();
                if !stmt.is_empty() {
                    match conn.execute_batch(stmt) {
                        Ok(()) => {}
                        Err(e) => {
                            let msg = e.to_string();
                            if !msg.contains("duplicate column") {
                                eprintln!("migration warning: {msg}");
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Public accessor
    // -----------------------------------------------------------------------

    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    // -----------------------------------------------------------------------
    // files
    // -----------------------------------------------------------------------

    /// Upsert a file row. Returns the id of the inserted/replaced row.
    pub fn upsert_file(
        &self,
        path: &str,
        language: &str,
        content_hash: &str,
        line_count: i64,
        parsed_ok: bool,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO files (path, language, content_hash, line_count, parsed_ok, last_parsed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET
                language     = excluded.language,
                content_hash = excluded.content_hash,
                line_count   = excluded.line_count,
                parsed_ok    = excluded.parsed_ok,
                last_parsed  = excluded.last_parsed",
            params![
                path,
                language,
                content_hash,
                line_count,
                parsed_ok as i64,
                now
            ],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Fetch a single file row by id.
    pub fn file_by_id(&self, id: i64) -> Result<Option<FileRow>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT id, path, language, content_hash, line_count, parsed_ok,
                    last_parsed, fan_in_files, blast_radius, pagerank
             FROM files WHERE id = ?1",
            params![id],
            map_file_row,
        ) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Fetch a single file row by path.
    pub fn file_by_path(&self, path: &str) -> Result<Option<FileRow>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT id, path, language, content_hash, line_count, parsed_ok,
                    last_parsed, fan_in_files, blast_radius, pagerank
             FROM files WHERE path = ?1",
            params![path],
            map_file_row,
        ) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Return all file rows ordered by blast_radius DESC (a rough proxy for
    /// symbol count / importance until we have real PageRank).
    pub fn all_files(&self) -> Result<Vec<FileRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, path, language, content_hash, line_count, parsed_ok,
                    last_parsed, fan_in_files, blast_radius, pagerank
             FROM files
             ORDER BY blast_radius DESC",
        )?;
        let rows: rusqlite::Result<Vec<FileRow>> = stmt.query_map([], map_file_row)?.collect();
        Ok(rows?)
    }

    /// Update the fan-in and blast-radius rollup columns for a file.
    pub fn update_rollups(&self, file_id: i64, fan_in: i64, blast_radius: i64) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE files SET fan_in_files = ?1, blast_radius = ?2 WHERE id = ?3",
            params![fan_in, blast_radius, file_id],
        )?;
        Ok(())
    }

    pub fn batch_update_file_pagerank(&self, updates: &[(i64, f64)]) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = conn.prepare_cached("UPDATE files SET pagerank = ?1 WHERE id = ?2")?;
            for &(file_id, pr) in updates {
                stmt.execute(params![pr, file_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn batch_update_symbol_pagerank(&self, updates: &[(i64, f64)]) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = conn.prepare_cached("UPDATE symbols SET pagerank = ?1 WHERE id = ?2")?;
            for &(sym_id, pr) in updates {
                stmt.execute(params![pr, sym_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete a file row. Foreign-key cascades remove symbols and refs.
    /// FTS5 rows must be deleted manually first.
    pub fn delete_file_cascade(&self, file_id: i64) -> Result<()> {
        let conn = self.conn.lock();

        // Manual FTS5 sync: delete every symbol FTS row for this file.
        let symbol_ids: Vec<i64> = {
            let mut stmt = conn.prepare("SELECT id FROM symbols WHERE file_id = ?1")?;
            let ids: rusqlite::Result<Vec<i64>> = stmt
                .query_map(params![file_id], |row| row.get(0))?
                .collect();
            ids?
        };

        for sid in &symbol_ids {
            conn.execute("DELETE FROM symbols_fts WHERE symbol_id = ?1", params![sid])?;
        }

        // Delete the file; FK cascades handle symbols and refs.
        conn.execute("DELETE FROM files WHERE id = ?1", params![file_id])?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // symbols
    // -----------------------------------------------------------------------

    pub fn insert_symbol(&self, p: &InsertSymbolParams<'_>) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO symbols (
                file_id, qualified_name, short_name, kind,
                signature, signature_hash, visibility,
                start_line, start_col, end_line, end_col,
                parent_symbol_id, docstring, cyclomatic, cognitive, flags
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                p.file_id,
                p.qualified_name,
                p.short_name,
                p.kind,
                p.signature,
                p.signature_hash,
                p.visibility,
                p.start_line,
                p.start_col,
                p.end_line,
                p.end_col,
                p.parent_symbol_id,
                p.docstring,
                p.cyclomatic,
                p.cognitive,
                p.flags,
            ],
        )?;
        let id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO symbols_fts (symbol_id, short_name, qualified_name, docstring)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, p.short_name, p.qualified_name, p.docstring],
        )?;

        Ok(id)
    }

    /// Fetch a symbol by its id.
    pub fn symbol_by_id(&self, id: i64) -> Result<Option<SymbolRow>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT id, file_id, qualified_name, short_name, kind,
                    signature, signature_hash, visibility,
                    start_line, start_col, end_line, end_col,
                    parent_symbol_id, docstring, pagerank,
                    cyclomatic, cognitive, flags
             FROM symbols WHERE id = ?1",
            params![id],
            map_symbol_row,
        ) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Fetch a symbol by its fully qualified name.
    pub fn symbol_by_qualified_name(&self, name: &str) -> Result<Option<SymbolRow>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT id, file_id, qualified_name, short_name, kind,
                    signature, signature_hash, visibility,
                    start_line, start_col, end_line, end_col,
                    parent_symbol_id, docstring, pagerank,
                    cyclomatic, cognitive, flags
             FROM symbols WHERE qualified_name = ?1",
            params![name],
            map_symbol_row,
        ) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Find symbols by name. Tries an exact match on `short_name` first;
    /// falls back to an FTS5 fuzzy match. Optionally filters by `kind`.
    /// Returns at most `limit` results.
    pub fn find_symbols_by_name(
        &self,
        name: &str,
        kind_filter: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SymbolRow>> {
        self.find_symbols_by_name_tiered(name, kind_filter, limit)
            .map(|(rows, _tier)| rows)
    }

    /// Like `find_symbols_by_name` but also returns which search tier matched.
    pub fn find_symbols_by_name_tiered(
        &self,
        name: &str,
        kind_filter: Option<&str>,
        limit: i64,
    ) -> Result<(Vec<SymbolRow>, crate::freshness::SearchTier)> {
        use crate::freshness::SearchTier;
        let conn = self.conn.lock();

        // Exact short_name match.
        let exact: Vec<SymbolRow> = {
            let mut stmt = match kind_filter {
                Some(_) => conn.prepare(
                    "SELECT id, file_id, qualified_name, short_name, kind,
                            signature, signature_hash, visibility,
                            start_line, start_col, end_line, end_col,
                            parent_symbol_id, docstring, pagerank,
                            cyclomatic, cognitive, flags
                     FROM symbols
                     WHERE short_name = ?1 AND kind = ?2
                     LIMIT ?3",
                )?,
                None => conn.prepare(
                    "SELECT id, file_id, qualified_name, short_name, kind,
                            signature, signature_hash, visibility,
                            start_line, start_col, end_line, end_col,
                            parent_symbol_id, docstring, pagerank,
                            cyclomatic, cognitive, flags
                     FROM symbols
                     WHERE short_name = ?1
                     LIMIT ?2",
                )?,
            };
            let rows: rusqlite::Result<Vec<SymbolRow>> = match kind_filter {
                Some(k) => stmt
                    .query_map(params![name, k, limit], map_symbol_row)?
                    .collect(),
                None => stmt
                    .query_map(params![name, limit], map_symbol_row)?
                    .collect(),
            };
            rows?
        };

        if !exact.is_empty() {
            return Ok((exact, SearchTier::Exact));
        }

        let escaped = name.replace('"', "\"\"");
        let fts_query = format!("\"{escaped}\"*");
        let ids: Vec<i64> = {
            let mut stmt = conn.prepare(
                "SELECT symbol_id FROM symbols_fts
                 WHERE symbols_fts MATCH ?1
                 LIMIT ?2",
            )?;
            let ids: rusqlite::Result<Vec<i64>> = stmt
                .query_map(params![fts_query, limit], |row| row.get(0))?
                .collect();
            ids?
        };

        if ids.is_empty() {
            return Ok((vec![], SearchTier::Fts));
        }

        // Fetch full rows for matched ids, respecting kind filter.
        let mut results = Vec::with_capacity(ids.len());
        for sid in ids {
            if let Some(sym) = {
                match conn.query_row(
                    "SELECT id, file_id, qualified_name, short_name, kind,
                            signature, signature_hash, visibility,
                            start_line, start_col, end_line, end_col,
                            parent_symbol_id, docstring, pagerank,
                            cyclomatic, cognitive, flags
                     FROM symbols WHERE id = ?1",
                    params![sid],
                    map_symbol_row,
                ) {
                    Ok(row) => Ok(Some(row)),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                    Err(e) => Err(SutraError::Db(e)),
                }?
            } && kind_filter.is_none_or(|k| sym.kind == k)
            {
                results.push(sym);
            }
        }
        Ok((results, SearchTier::Fts))
    }

    /// Return all symbols in a file ordered by start_line.
    pub fn find_symbols_by_file(&self, file_id: i64) -> Result<Vec<SymbolRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, qualified_name, short_name, kind,
                    signature, signature_hash, visibility,
                    start_line, start_col, end_line, end_col,
                    parent_symbol_id, docstring, pagerank,
                    cyclomatic, cognitive, flags
             FROM symbols
             WHERE file_id = ?1
             ORDER BY start_line",
        )?;
        let rows: rusqlite::Result<Vec<SymbolRow>> =
            stmt.query_map(params![file_id], map_symbol_row)?.collect();
        Ok(rows?)
    }

    /// Return (file_id, symbol_count) for all files in a single query.
    pub fn symbol_counts_by_file(&self) -> Result<std::collections::HashMap<i64, i64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT file_id, COUNT(*) FROM symbols GROUP BY file_id")?;
        let rows: rusqlite::Result<Vec<(i64, i64)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect();
        Ok(rows?.into_iter().collect())
    }

    /// Return (file_id, (max_cognitive, avg_cognitive)) for files with complexity data.
    pub fn complexity_by_file(&self) -> Result<std::collections::HashMap<i64, (i64, f64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT file_id, MAX(cognitive), AVG(cognitive)
             FROM symbols
             WHERE cognitive IS NOT NULL
             GROUP BY file_id",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64, f64)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect();
        Ok(rows?
            .into_iter()
            .map(|(fid, max_c, avg_c)| (fid, (max_c, avg_c)))
            .collect())
    }

    /// Find symbols with zero inbound references (potential dead code).
    /// Returns (qualified_name, file_path, kind, start_line, visibility).
    pub fn find_dead_symbols(
        &self,
        include_pub: bool,
        path_prefix: Option<&str>,
    ) -> Result<Vec<(String, String, String, i64, Option<String>)>> {
        let conn = self.conn.lock();
        let like_pattern = path_prefix.map(|p| format!("{p}%"));
        let mut stmt = conn.prepare(
            "SELECT s.qualified_name, f.path, s.kind, s.start_line, s.visibility
             FROM symbols s
             JOIN files f ON s.file_id = f.id
             LEFT JOIN refs r ON r.target_symbol_id = s.id
             WHERE r.id IS NULL
               AND s.kind IN ('function','method','struct','enum','trait',
                              'type_alias','class','mixin','const','static')
               AND s.short_name != 'main'
               AND (s.flags & 7) = 0
               AND f.path NOT LIKE 'tests/%'
               AND (?1 = 1 OR s.visibility IS NULL OR s.visibility NOT IN ('pub','public'))
               AND (?2 IS NULL OR f.path LIKE ?2)
             ORDER BY f.path, s.start_line",
        )?;
        let rows: rusqlite::Result<Vec<_>> = stmt
            .query_map(params![include_pub as i32, like_pattern], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })?
            .collect();
        Ok(rows?)
    }

    /// Return dead-symbol ratio (0.0–1.0) per file.
    pub fn dead_symbol_ratio_by_file(&self) -> Result<std::collections::HashMap<i64, f64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT s.file_id,
                    SUM(CASE WHEN r.id IS NULL THEN 1 ELSE 0 END) AS dead,
                    COUNT(*) AS total
             FROM symbols s
             LEFT JOIN refs r ON r.target_symbol_id = s.id
             JOIN files f ON s.file_id = f.id
             WHERE s.kind IN ('function','method','struct','enum','trait',
                              'type_alias','class','mixin','const','static')
               AND (s.flags & 7) = 0
               AND f.path NOT LIKE 'tests/%'
             GROUP BY s.file_id",
        )?;
        let rows: rusqlite::Result<Vec<(i64, f64, f64)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect();
        Ok(rows?
            .into_iter()
            .map(|(fid, dead, total)| {
                let ratio = if total > 0.0 { dead / total } else { 0.0 };
                (fid, ratio)
            })
            .collect())
    }

    /// Find files with zero fan-in that are not root files.
    /// Returns (path, line_count).
    pub fn find_unreachable_files(&self, path_prefix: Option<&str>) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock();
        let like_pattern = path_prefix.map(|p| format!("{p}%"));
        let mut stmt = conn.prepare(
            "SELECT path, line_count FROM files
             WHERE fan_in_files = 0
               AND path NOT LIKE '%/lib.rs'
               AND path NOT LIKE '%/main.rs'
               AND path NOT LIKE '%/mod.rs'
               AND path NOT LIKE 'src/bin/%'
               AND path NOT LIKE 'lib/%'
               AND path NOT LIKE 'tests/%'
               AND (?1 IS NULL OR path LIKE ?1)
             ORDER BY path",
        )?;
        let rows: rusqlite::Result<Vec<(String, i64)>> = stmt
            .query_map(params![like_pattern], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect();
        Ok(rows?)
    }

    /// Load all (id, qualified_name, short_name, kind) tuples in a single query.
    pub fn all_symbols_summary(&self) -> Result<Vec<(i64, String, String, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, qualified_name, short_name, kind FROM symbols")?;
        let rows: rusqlite::Result<Vec<(i64, String, String, String)>> = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect();
        Ok(rows?)
    }

    /// Resolve a symbol by name: try qualified_name first, then short_name lookup.
    pub fn resolve_symbol(&self, name: &str, kind: Option<&str>) -> Result<Option<SymbolRow>> {
        if let Some(sym) = self.symbol_by_qualified_name(name)? {
            return Ok(Some(sym));
        }
        let mut results = self.find_symbols_by_name(name, kind, 1)?;
        Ok(if results.is_empty() {
            None
        } else {
            Some(results.swap_remove(0))
        })
    }

    /// Find the narrowest symbol enclosing the given line in a file.
    pub fn find_enclosing_symbol(&self, file_id: i64, line: i64) -> Result<Option<SymbolRow>> {
        let symbols = self.find_symbols_by_file(file_id)?;
        let mut best: Option<&SymbolRow> = None;
        for s in &symbols {
            if s.start_line <= line && line <= s.end_line {
                match best {
                    None => best = Some(s),
                    Some(prev)
                        if (s.end_line - s.start_line) < (prev.end_line - prev.start_line) =>
                    {
                        best = Some(s);
                    }
                    _ => {}
                }
            }
        }
        Ok(best.cloned())
    }

    // -----------------------------------------------------------------------
    // refs
    // -----------------------------------------------------------------------

    /// Insert a reference row. Returns the new row id.
    pub fn insert_ref(
        &self,
        file_id: i64,
        target_symbol_id: Option<i64>,
        unresolved_name: Option<&str>,
        line: i64,
        col: i64,
        context_kind: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO refs (file_id, target_symbol_id, unresolved_name, line, col, context_kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                file_id,
                target_symbol_id,
                unresolved_name,
                line,
                col,
                context_kind
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Return all refs that target a given symbol.
    pub fn find_refs_to_symbol(&self, symbol_id: i64) -> Result<Vec<RefRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, target_symbol_id, unresolved_name, line, col, context_kind
             FROM refs WHERE target_symbol_id = ?1",
        )?;
        let rows: rusqlite::Result<Vec<RefRow>> =
            stmt.query_map(params![symbol_id], map_ref_row)?.collect();
        Ok(rows?)
    }

    /// Return all refs contained in a given file.
    pub fn find_refs_in_file(&self, file_id: i64) -> Result<Vec<RefRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, target_symbol_id, unresolved_name, line, col, context_kind
             FROM refs WHERE file_id = ?1",
        )?;
        let rows: rusqlite::Result<Vec<RefRow>> =
            stmt.query_map(params![file_id], map_ref_row)?.collect();
        Ok(rows?)
    }

    /// Delete all refs belonging to a given file.
    pub fn delete_refs_by_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .lock()
            .execute("DELETE FROM refs WHERE file_id = ?1", params![file_id])?;
        Ok(())
    }

    /// Return the distinct set of file_ids that contain a reference to any of
    /// the given symbol_ids.
    pub fn find_files_referencing_symbols(&self, symbol_ids: &[i64]) -> Result<Vec<i64>> {
        if symbol_ids.is_empty() {
            return Ok(vec![]);
        }

        // Build a parameterised IN clause at runtime.
        let placeholders: String = symbol_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");

        let sql =
            format!("SELECT DISTINCT file_id FROM refs WHERE target_symbol_id IN ({placeholders})");

        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let ids: rusqlite::Result<Vec<i64>> = stmt
            .query_map(rusqlite::params_from_iter(symbol_ids.iter()), |row| {
                row.get(0)
            })?
            .collect();
        Ok(ids?)
    }

    // -----------------------------------------------------------------------
    // imports
    // -----------------------------------------------------------------------

    /// Insert an import row. Returns the new row id.
    pub fn insert_import(
        &self,
        file_id: i64,
        imported_path: &str,
        resolved_file_id: Option<i64>,
        line: i64,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO imports (file_id, imported_path, resolved_file_id, line)
             VALUES (?1, ?2, ?3, ?4)",
            params![file_id, imported_path, resolved_file_id, line],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Return all import rows for a file.
    pub fn imports_for_file(&self, file_id: i64) -> Result<Vec<ImportRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, imported_path, resolved_file_id, line
             FROM imports WHERE file_id = ?1",
        )?;
        let rows: rusqlite::Result<Vec<ImportRow>> =
            stmt.query_map(params![file_id], map_import_row)?.collect();
        Ok(rows?)
    }

    /// Return all resolved import edges: (file_id, resolved_file_id) pairs
    /// where resolved_file_id IS NOT NULL.
    pub fn import_edges(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT file_id, resolved_file_id FROM imports WHERE resolved_file_id IS NOT NULL",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect();
        Ok(rows?)
    }

    /// Batch: return (symbol_id, file_id) for every symbol.
    pub fn all_symbol_file_map(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, file_id FROM symbols")?;
        let rows: rusqlite::Result<Vec<(i64, i64)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect();
        Ok(rows?)
    }

    /// Batch: return (file_id, target_symbol_id) for every resolved ref.
    pub fn all_resolved_refs(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT file_id, target_symbol_id FROM refs WHERE target_symbol_id IS NOT NULL",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect();
        Ok(rows?)
    }

    // -----------------------------------------------------------------------
    // snapshots
    // -----------------------------------------------------------------------

    /// Insert a snapshot record. Returns the new row id.
    pub fn insert_snapshot(
        &self,
        files_parsed: i64,
        symbols_extracted: i64,
        refs_extracted: i64,
        parse_errors: i64,
        duration_ms: i64,
        total_complexity: i64,
        dead_symbol_count: i64,
        hotspot_count: i64,
        health_score: i64,
    ) -> Result<i64> {
        let now = chrono::Utc::now().to_rfc3339();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO snapshots (timestamp, files_parsed, symbols_extracted,
                                    refs_extracted, parse_errors, duration_ms,
                                    total_complexity, dead_symbol_count,
                                    hotspot_count, health_score)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                now,
                files_parsed,
                symbols_extracted,
                refs_extracted,
                parse_errors,
                duration_ms,
                total_complexity,
                dead_symbol_count,
                hotspot_count,
                health_score,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Return the timestamp of the most recent snapshot, or `None` if no
    /// snapshot has been recorded yet.
    pub fn last_parse_time(&self) -> Result<Option<String>> {
        let conn = self.conn.lock();
        match conn.query_row("SELECT MAX(timestamp) FROM snapshots", [], |row| {
            row.get::<_, Option<String>>(0)
        }) {
            Ok(ts) => Ok(ts),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(SutraError::Db(e)),
        }
    }

    /// Return the N most recent snapshots, ordered newest-first.
    pub fn latest_snapshots(&self, limit: i64) -> Result<Vec<SnapshotRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, files_parsed, symbols_extracted,
                    refs_extracted, parse_errors, duration_ms,
                    total_complexity, dead_symbol_count,
                    hotspot_count, health_score
             FROM snapshots ORDER BY timestamp DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit], map_snapshot_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Return all snapshots whose timestamp falls within [from, to], ordered oldest-first.
    pub fn snapshots_between(&self, from: &str, to: &str) -> Result<Vec<SnapshotRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, timestamp, files_parsed, symbols_extracted,
                    refs_extracted, parse_errors, duration_ms,
                    total_complexity, dead_symbol_count,
                    hotspot_count, health_score
             FROM snapshots WHERE timestamp >= ?1 AND timestamp <= ?2
             ORDER BY timestamp ASC",
        )?;
        let rows = stmt
            .query_map(params![from, to], map_snapshot_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Row mappers
// ---------------------------------------------------------------------------

fn map_file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRow> {
    let parsed_ok_int: i64 = row.get(5)?;
    Ok(FileRow {
        id: row.get(0)?,
        path: row.get(1)?,
        language: row.get(2)?,
        content_hash: row.get(3)?,
        line_count: row.get(4)?,
        parsed_ok: parsed_ok_int != 0,
        last_parsed: row.get(6)?,
        fan_in_files: row.get(7)?,
        blast_radius: row.get(8)?,
        pagerank: row.get(9)?,
    })
}

fn map_symbol_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SymbolRow> {
    Ok(SymbolRow {
        id: row.get(0)?,
        file_id: row.get(1)?,
        qualified_name: row.get(2)?,
        short_name: row.get(3)?,
        kind: row.get(4)?,
        signature: row.get(5)?,
        signature_hash: row.get(6)?,
        visibility: row.get(7)?,
        start_line: row.get(8)?,
        start_col: row.get(9)?,
        end_line: row.get(10)?,
        end_col: row.get(11)?,
        parent_symbol_id: row.get(12)?,
        docstring: row.get(13)?,
        pagerank: row.get(14)?,
        cyclomatic: row.get(15)?,
        cognitive: row.get(16)?,
        flags: row.get(17)?,
    })
}

fn map_ref_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RefRow> {
    Ok(RefRow {
        id: row.get(0)?,
        file_id: row.get(1)?,
        target_symbol_id: row.get(2)?,
        unresolved_name: row.get(3)?,
        line: row.get(4)?,
        col: row.get(5)?,
        context_kind: row.get(6)?,
    })
}

fn map_import_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportRow> {
    Ok(ImportRow {
        id: row.get(0)?,
        file_id: row.get(1)?,
        imported_path: row.get(2)?,
        resolved_file_id: row.get(3)?,
        line: row.get(4)?,
    })
}

fn map_snapshot_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotRow> {
    Ok(SnapshotRow {
        id: row.get(0)?,
        timestamp: row.get(1)?,
        files_parsed: row.get(2)?,
        symbols_extracted: row.get(3)?,
        refs_extracted: row.get(4)?,
        parse_errors: row.get(5)?,
        duration_ms: row.get(6)?,
        total_complexity: row.get(7)?,
        dead_symbol_count: row.get(8)?,
        hotspot_count: row.get(9)?,
        health_score: row.get(10)?,
    })
}
