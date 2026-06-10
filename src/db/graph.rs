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

    pub fn cochange_for_file(
        &self,
        file_id: i64,
        threshold: f64,
    ) -> Result<Vec<(String, f64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "WITH target_commits AS (
                SELECT commit_hash FROM commit_files WHERE file_id = ?1
            ),
            target_count AS (
                SELECT COUNT(*) AS cnt FROM target_commits
            ),
            cochanged AS (
                SELECT cf.file_id, COUNT(*) AS shared_cnt
                FROM commit_files cf
                JOIN target_commits tc ON cf.commit_hash = tc.commit_hash
                WHERE cf.file_id != ?1
                GROUP BY cf.file_id
            ),
            file_counts AS (
                SELECT file_id, COUNT(*) AS cnt FROM commit_files GROUP BY file_id
            )
            SELECT f.path, c.shared_cnt,
                   CAST(c.shared_cnt AS REAL) / (tc.cnt + fc.cnt - c.shared_cnt) AS jaccard
            FROM cochanged c
            JOIN files f ON f.id = c.file_id
            JOIN target_count tc
            JOIN file_counts fc ON fc.file_id = c.file_id
            WHERE CAST(c.shared_cnt AS REAL) / (tc.cnt + fc.cnt - c.shared_cnt) >= ?2
            ORDER BY jaccard DESC",
        )?;
        let rows: rusqlite::Result<Vec<(String, f64, i64)>> = stmt
            .query_map(params![file_id, threshold], |row| {
                let path: String = row.get(0)?;
                let shared: i64 = row.get(1)?;
                let jaccard: f64 = row.get(2)?;
                Ok((path, jaccard, shared))
            })?
            .collect();
        Ok(rows?)
    }

    pub fn commit_file_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM commit_files", [], |r| r.get(0))?)
    }

    pub fn newest_commit_at(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row(
            "SELECT COALESCE(MAX(committed_at), 0) FROM commits",
            [],
            |r| r.get(0),
        )?)
    }

    pub fn static_file_edges(&self) -> Result<Vec<(i64, i64)>> {
        use std::collections::{HashMap, HashSet};
        let sym_file: HashMap<i64, i64> = self.all_symbol_file_map()?.into_iter().collect();
        let refs = self.all_resolved_refs()?;
        let mut edges = HashSet::new();
        for (src_file, target_sym) in refs {
            if let Some(&target_file) = sym_file.get(&target_sym)
                && src_file != target_file
            {
                edges.insert((src_file.min(target_file), src_file.max(target_file)));
            }
        }
        Ok(edges.into_iter().collect())
    }

    pub fn file_cochange_partners(&self) -> Result<Vec<(i64, i64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT cf1.file_id,
                    COUNT(DISTINCT cf2.file_id) AS partner_count,
                    COUNT(DISTINCT cf1.commit_hash) AS cochange_commit_count
             FROM commit_files cf1
             JOIN commit_files cf2
               ON cf1.commit_hash = cf2.commit_hash AND cf1.file_id != cf2.file_id
             GROUP BY cf1.file_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn file_commit_sizes(&self, max_width: i64) -> Result<Vec<(i64, i64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "WITH commit_size AS (
                 SELECT commit_hash, COUNT(*) AS file_count
                 FROM commit_files GROUP BY commit_hash
                 HAVING file_count <= ?1
             )
             SELECT cf.file_id, c.committed_at, cs.file_count
             FROM commit_files cf
             JOIN commits c ON cf.commit_hash = c.hash
             JOIN commit_size cs ON cs.commit_hash = cf.commit_hash",
        )?;
        let rows = stmt.query_map([max_width], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn file_author_commits(&self) -> Result<Vec<(i64, String, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT cf.file_id, c.author, COUNT(*) AS commit_count
             FROM commit_files cf
             JOIN commits c ON cf.commit_hash = c.hash
             GROUP BY cf.file_id, c.author",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
}
