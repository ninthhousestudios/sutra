use std::collections::HashMap;

use rusqlite::OptionalExtension;
use rusqlite::params;

use super::Db;
use crate::error::Result;
use crate::similarity::hrr::HrrVec;

pub struct PatternFamily {
    pub member_symbol_ids: Vec<i64>,
    pub avg_similarity: f64,
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
    pub file_path: String,
    pub language: String,
    pub start_line: i64,
    pub start_col: i64,
    pub end_line: i64,
    pub end_col: i64,
}

impl Db {
    pub fn function_symbols_for_hrr(&self) -> Result<Vec<HrrSymbolRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT s.id, f.path, f.language,
                    s.start_line, s.start_col, s.end_line, s.end_col
             FROM symbols s
             JOIN files f ON s.file_id = f.id
             WHERE s.kind IN ('function', 'method')",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(HrrSymbolRow {
                    symbol_id: row.get(0)?,
                    file_path: row.get(1)?,
                    language: row.get(2)?,
                    start_line: row.get(3)?,
                    start_col: row.get(4)?,
                    end_line: row.get(5)?,
                    end_col: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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

    pub fn load_hrr_codebook(&self) -> Result<HashMap<String, HrrVec>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT key, vector FROM hrr_codebook")?;
        let mut entries = HashMap::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            entries.insert(key, HrrVec::from_bytes(&blob));
        }
        Ok(entries)
    }

    pub fn save_hrr_codebook_entries(&self, entries: &[(String, Vec<u8>)]) -> Result<usize> {
        if entries.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("INSERT OR IGNORE INTO hrr_codebook (key, vector) VALUES (?1, ?2)")?;
        for (key, blob) in entries {
            stmt.execute(params![key, blob])?;
        }
        Ok(entries.len())
    }

    pub fn load_all_strip_vectors(&self) -> Result<Vec<(i64, HrrVec)>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT symbol_id, vector FROM hrr_vectors WHERE mode = 'strip'")?;
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
        let mut stmt = conn.prepare("SELECT symbol_id, vector FROM hrr_vectors WHERE mode = ?1")?;
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
                "INSERT INTO pattern_families (member_count, avg_similarity) VALUES (?1, ?2)",
            )?;
            let mut mem_stmt = conn.prepare(
                "INSERT INTO pattern_family_members (family_id, symbol_id) VALUES (?1, ?2)",
            )?;

            for family in families {
                fam_stmt.execute(params![
                    family.member_symbol_ids.len() as i64,
                    family.avg_similarity,
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

    pub fn strip_vectors_by_component(&self) -> Result<Vec<(String, i64, Vec<u8>)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT cm.component_id, hv.symbol_id, hv.vector
             FROM hrr_vectors hv
             JOIN symbols s ON s.id = hv.symbol_id
             JOIN component_membership cm ON cm.file_id = s.file_id
             JOIN components c ON c.id = cm.component_id
             WHERE hv.mode = 'strip' AND c.dissolved_at IS NULL
             ORDER BY cm.component_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
