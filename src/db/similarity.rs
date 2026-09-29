use rusqlite::OptionalExtension;
use rusqlite::params;

use super::Db;
use crate::error::Result;
use crate::similarity::hrr::HrrVec;

pub struct SymbolSummary {
    pub id: i64,
    pub qualified_name: String,
    pub file_path: String,
    pub start_line: i64,
    pub end_line: i64,
}

pub struct HrrSymbolRow {
    pub symbol_id: i64,
    pub file_id: i64,
    pub file_path: String,
    pub language: String,
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: i64,
    pub end_col: i64,
}

/// A function or method as the review-time dup check reads it
/// (`tools::dup_exists`): the encoder's row plus identity and flags.
pub struct CorpusFunction {
    pub hrr: HrrSymbolRow,
    pub qualified_name: String,
    pub short_name: String,
    pub kind: String,
    pub flags: i64,
}

pub struct HrrChangedFile {
    pub file_id: i64,
    pub path: String,
    pub language: String,
    pub content_hash: String,
}

/// Function/method rows the HRR encoder reads; callers may append `AND ...`.
const HRR_SYMBOL_SELECT: &str = "SELECT s.id, s.file_id, f.path, f.language,
        s.start_line, s.start_col, s.end_line, s.end_col
 FROM symbols s
 JOIN files f ON s.file_id = f.id
 WHERE s.kind IN ('function', 'method')";

fn hrr_symbol_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HrrSymbolRow> {
    Ok(HrrSymbolRow {
        symbol_id: row.get(0)?,
        file_id: row.get(1)?,
        file_path: row.get(2)?,
        language: row.get(3)?,
        start_line: row.get(4)?,
        start_col: row.get(5)?,
        end_line: row.get(6)?,
        end_col: row.get(7)?,
    })
}

impl Db {
    /// Every function and method, in id order.
    pub fn function_corpus(&self) -> Result<Vec<CorpusFunction>> {
        let conn = self.conn.lock();
        let sql = HRR_SYMBOL_SELECT.replacen(
            "\n FROM symbols s",
            ", s.qualified_name, s.short_name, s.kind, s.flags\n FROM symbols s",
            1,
        ) + " ORDER BY s.id";
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CorpusFunction {
                    hrr: hrr_symbol_row(row)?,
                    qualified_name: row.get(8)?,
                    short_name: row.get(9)?,
                    kind: row.get(10)?,
                    flags: row.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn function_symbols_for_hrr(&self) -> Result<Vec<HrrSymbolRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(HRR_SYMBOL_SELECT)?;
        let rows = stmt
            .query_map([], hrr_symbol_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn function_symbol_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM symbols WHERE kind IN ('function', 'method')",
            [],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    /// Cheap (idx_hrr_vectors_mode-backed) probe so strip-only mode can skip the
    /// embed purge — and its implicit write transaction — on every incremental
    /// parse once the embed vectors are already gone (sutra/328).
    pub fn has_embed_vectors(&self) -> Result<bool> {
        let conn = self.conn.lock();
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM hrr_vectors WHERE mode = 'embed')",
            [],
            |r| r.get(0),
        )?;
        Ok(exists)
    }

    pub fn delete_embed_vectors(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let deleted = conn.execute("DELETE FROM hrr_vectors WHERE mode = 'embed'", [])?;
        Ok(deleted)
    }

    pub fn function_symbols_for_hrr_files(&self, file_ids: &[i64]) -> Result<Vec<HrrSymbolRow>> {
        if file_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let placeholders: String = file_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("{HRR_SYMBOL_SELECT} AND f.id IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = file_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt
            .query_map(params.as_slice(), hrr_symbol_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn files_needing_hrr_recompute(&self) -> Result<Vec<HrrChangedFile>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT f.id, f.path, f.language, f.content_hash
             FROM files f
             JOIN symbols s ON s.file_id = f.id
             WHERE s.kind IN ('function', 'method')
               AND (NOT EXISTS (SELECT 1 FROM hrr_file_hashes h WHERE h.file_id = f.id)
                    OR (SELECT h.content_hash FROM hrr_file_hashes h WHERE h.file_id = f.id) != f.content_hash)",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(HrrChangedFile {
                    file_id: row.get(0)?,
                    path: row.get(1)?,
                    language: row.get(2)?,
                    content_hash: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn insert_hrr_vectors_and_hashes(
        &self,
        vectors: &[(i64, &str, &[u8])],
        file_hashes: &[(i64, &str)],
    ) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            let mut vec_stmt = conn.prepare(
                "INSERT OR REPLACE INTO hrr_vectors (symbol_id, mode, vector) VALUES (?1, ?2, ?3)",
            )?;
            for &(sym_id, mode, blob) in vectors {
                vec_stmt.execute(params![sym_id, mode, blob])?;
            }

            let mut hash_stmt = conn.prepare(
                "INSERT OR REPLACE INTO hrr_file_hashes (file_id, content_hash) VALUES (?1, ?2)",
            )?;
            for &(file_id, hash) in file_hashes {
                hash_stmt.execute(params![file_id, hash])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_hrr_vector(&self, symbol_id: i64, mode: &str) -> Result<Option<HrrVec>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT vector FROM hrr_vectors WHERE symbol_id = ?1 AND mode = ?2")?;
        let result = stmt
            .query_row(params![symbol_id, mode], |row| {
                let blob: Vec<u8> = row.get(0)?;
                Ok(HrrVec::from_bytes(&blob))
            })
            .optional()?;
        Ok(result)
    }

    pub fn load_all_vectors_by_mode(&self, mode: &str) -> Result<Vec<(i64, HrrVec)>> {
        let conn = self.conn.lock();
        // ORDER BY: stable input order so family detection is deterministic
        // (sutra/327) and ranked search breaks exact-cosine ties the same way
        // run-to-run (sutra/328).
        let mut stmt = conn.prepare(
            "SELECT symbol_id, vector FROM hrr_vectors WHERE mode = ?1 ORDER BY symbol_id",
        )?;
        let rows = stmt
            .query_map(params![mode], |row| {
                let id: i64 = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                Ok((id, HrrVec::from_bytes(&blob)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn symbols_by_ids(&self, ids: &[i64]) -> Result<Vec<SymbolSummary>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT s.id, s.qualified_name, f.path, s.start_line, s.end_line
             FROM symbols s
             JOIN files f ON s.file_id = f.id
             WHERE s.id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                Ok(SymbolSummary {
                    id: row.get(0)?,
                    qualified_name: row.get(1)?,
                    file_path: row.get(2)?,
                    start_line: row.get(3)?,
                    end_line: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
