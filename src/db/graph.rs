use rusqlite::params;

use crate::error::Result;

use super::{CommitRow, Db};

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

    pub fn replace_commit_files(
        &self,
        commits: &[CommitRow],
        pairs: &[(String, i64)],
    ) -> Result<usize> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            conn.execute_batch("DELETE FROM commit_files; DELETE FROM commits")?;
            let mut commit_stmt = conn.prepare_cached(
                "INSERT OR IGNORE INTO commits (hash, committed_at, author) VALUES (?1, ?2, ?3)",
            )?;
            for c in commits {
                commit_stmt.execute(params![c.hash, c.committed_at, c.author])?;
            }
            let mut cf_stmt = conn.prepare_cached(
                "INSERT OR IGNORE INTO commit_files (commit_hash, file_id) VALUES (?1, ?2)",
            )?;
            for (hash, file_id) in pairs {
                cf_stmt.execute(params![hash, file_id])?;
            }
        }
        tx.commit()?;
        Ok(pairs.len())
    }

    pub fn cochange_pairs_above_threshold(
        &self,
        threshold: f64,
    ) -> Result<Vec<(i64, i64, f64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "WITH file_commit_counts AS (
                SELECT file_id, COUNT(*) AS cnt FROM commit_files GROUP BY file_id
            ),
            shared AS (
                SELECT a.file_id AS fa, b.file_id AS fb, COUNT(*) AS shared_cnt
                FROM commit_files a
                JOIN commit_files b ON a.commit_hash = b.commit_hash AND a.file_id < b.file_id
                GROUP BY a.file_id, b.file_id
            )
            SELECT s.fa, s.fb,
                   CAST(s.shared_cnt AS REAL) / (ca.cnt + cb.cnt - s.shared_cnt) AS jaccard,
                   s.shared_cnt
            FROM shared s
            JOIN file_commit_counts ca ON ca.file_id = s.fa
            JOIN file_commit_counts cb ON cb.file_id = s.fb
            WHERE CAST(s.shared_cnt AS REAL) / (ca.cnt + cb.cnt - s.shared_cnt) >= ?1",
        )?;
        let rows: rusqlite::Result<Vec<(i64, i64, f64, i64)>> = stmt
            .query_map(params![threshold], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect();
        Ok(rows?)
    }

    pub fn commit_file_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM commit_files", [], |r| r.get(0))?)
    }
}
