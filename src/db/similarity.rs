use std::collections::HashMap;

use rusqlite::params;

use super::Db;
use crate::error::Result;
use crate::similarity::hrr::HrrVec;

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
        conn.execute("DELETE FROM hrr_vectors", [])?;
        let mut stmt = conn.prepare(
            "INSERT INTO hrr_vectors (symbol_id, mode, vector) VALUES (?1, ?2, ?3)",
        )?;
        for &(sym_id, mode, blob) in vectors {
            stmt.execute(params![sym_id, mode, blob])?;
        }
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
}
