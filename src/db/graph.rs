use rusqlite::params;

use crate::error::Result;

use super::Db;

impl Db {
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

    pub fn all_symbol_file_map(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, file_id FROM symbols")?;
        let rows: rusqlite::Result<Vec<(i64, i64)>> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect();
        Ok(rows?)
    }

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
}
