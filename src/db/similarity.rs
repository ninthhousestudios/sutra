use rusqlite::OptionalExtension;
use rusqlite::params;

use super::Db;
use crate::error::Result;
use crate::similarity::hrr::HrrVec;

pub struct PatternFamily {
    pub member_symbol_ids: Vec<i64>,
    pub avg_similarity: f64,
    pub detection_mode: &'static str,
}

pub struct SymbolSummary {
    pub id: i64,
    pub qualified_name: String,
    pub file_path: String,
    pub start_line: i64,
    pub end_line: i64,
}

pub struct PatternFamilyMember {
    pub symbol_id: i64,
    pub qualified_name: String,
    pub file_path: String,
    pub start_line: i64,
    pub end_line: i64,
}

pub struct PatternFamilyRow {
    pub family_id: i64,
    pub member_count: i64,
    pub avg_similarity: f64,
    pub members: Vec<PatternFamilyMember>,
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

pub struct HrrChangedFile {
    pub file_id: i64,
    pub path: String,
    pub language: String,
    pub content_hash: String,
}

impl Db {
    pub fn function_symbols_for_hrr(&self) -> Result<Vec<HrrSymbolRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.file_id, f.path, f.language,
                    s.start_line, s.start_col, s.end_line, s.end_col
             FROM symbols s
             JOIN files f ON s.file_id = f.id
             WHERE s.kind IN ('function', 'method')",
        )?;
        let rows = stmt
            .query_map([], |row| {
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
            })?
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

    pub fn function_symbol_names(&self) -> Result<Vec<(i64, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT s.id, s.qualified_name
             FROM symbols s
             WHERE s.kind IN ('function', 'method')
             ORDER BY s.id",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn function_symbols_for_hrr_files(&self, file_ids: &[i64]) -> Result<Vec<HrrSymbolRow>> {
        if file_ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock();
        let placeholders: String = file_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT s.id, s.file_id, f.path, f.language,
                    s.start_line, s.start_col, s.end_line, s.end_col
             FROM symbols s
             JOIN files f ON s.file_id = f.id
             WHERE s.kind IN ('function', 'method') AND f.id IN ({placeholders})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = file_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt
            .query_map(params.as_slice(), |row| {
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
            })?
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

    pub fn replace_hrr_vectors(&self, vectors: &[(i64, &str, &[u8])]) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            conn.execute("DELETE FROM hrr_vectors", [])?;
            let mut stmt = conn
                .prepare("INSERT INTO hrr_vectors (symbol_id, mode, vector) VALUES (?1, ?2, ?3)")?;
            for &(sym_id, mode, blob) in vectors {
                stmt.execute(params![sym_id, mode, blob])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn hrr_vector_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM hrr_vectors", [], |r| r.get(0))?;
        Ok(count)
    }

    pub fn load_all_strip_vectors(&self) -> Result<Vec<(i64, HrrVec)>> {
        let conn = self.conn.lock();
        // ORDER BY: downstream family detection must see a stable input order
        // for run-to-run determinism (sutra/327).
        let mut stmt = conn.prepare(
            "SELECT symbol_id, vector FROM hrr_vectors WHERE mode = 'strip' ORDER BY symbol_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                Ok((id, HrrVec::from_bytes(&blob)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
        // ORDER BY: stable input order so ranked search breaks exact-cosine
        // ties the same way run-to-run (sutra/328, matches load_all_strip_vectors).
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

    pub fn replace_pattern_families(&self, families: &[PatternFamily]) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch("BEGIN")?;

        let result = (|| {
            conn.execute("DELETE FROM pattern_family_members", [])?;
            conn.execute("DELETE FROM pattern_families", [])?;

            let mut fam_stmt = conn.prepare(
                "INSERT INTO pattern_families (member_count, avg_similarity, detection_mode) VALUES (?1, ?2, ?3)",
            )?;
            let mut mem_stmt = conn.prepare(
                "INSERT INTO pattern_family_members (family_id, symbol_id) VALUES (?1, ?2)",
            )?;

            for family in families {
                fam_stmt.execute(params![
                    family.member_symbol_ids.len() as i64,
                    family.avg_similarity,
                    family.detection_mode,
                ])?;
                let family_id = conn.last_insert_rowid();
                for &sym_id in &family.member_symbol_ids {
                    mem_stmt.execute(params![family_id, sym_id])?;
                }
            }
            Ok(())
        })();

        if result.is_ok() {
            conn.execute_batch("COMMIT")?;
        } else {
            let _ = conn.execute_batch("ROLLBACK");
        }
        result
    }

    pub fn pattern_family_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM pattern_families", [], |r| r.get(0))?;
        Ok(count)
    }

    pub fn query_pattern_families(&self) -> Result<Vec<PatternFamilyRow>> {
        let conn = self.conn.lock();
        let mut fam_stmt =
            conn.prepare("SELECT id, member_count, avg_similarity FROM pattern_families ORDER BY member_count DESC")?;
        let mut mem_stmt = conn.prepare(
            "SELECT m.symbol_id, s.qualified_name, f.path, s.start_line, s.end_line
             FROM pattern_family_members m
             JOIN symbols s ON m.symbol_id = s.id
             JOIN files f ON s.file_id = f.id
             WHERE m.family_id = ?1
             ORDER BY f.path, s.start_line",
        )?;

        let mut result = Vec::new();
        let mut rows = fam_stmt.query([])?;
        while let Some(row) = rows.next()? {
            let family_id: i64 = row.get(0)?;
            let member_count: i64 = row.get(1)?;
            let avg_similarity: f64 = row.get(2)?;

            let members = mem_stmt
                .query_map(params![family_id], |r| {
                    Ok(PatternFamilyMember {
                        symbol_id: r.get(0)?,
                        qualified_name: r.get(1)?,
                        file_path: r.get(2)?,
                        start_line: r.get(3)?,
                        end_line: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            result.push(PatternFamilyRow {
                family_id,
                member_count,
                avg_similarity,
                members,
            });
        }
        Ok(result)
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
