use std::collections::HashSet;

use rusqlite::params;

use crate::error::Result;

use super::Db;

pub struct EntityChangeRow {
    pub qualified_name: String,
    pub kind: String,
    pub file_path: String,
    pub change_type: String,
    pub old_qualified_name: Option<String>,
    pub old_file_path: Option<String>,
}

const MAX_PAIR_ENTITIES: usize = 50;

impl Db {
    pub fn known_entity_commit_hashes(&self) -> Result<HashSet<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT hash FROM entity_commits")?;
        let rows: rusqlite::Result<HashSet<String>> =
            stmt.query_map([], |row| row.get(0))?.collect();
        Ok(rows?)
    }

    pub fn insert_entity_commit_with_changes(
        &self,
        hash: &str,
        committed_at: i64,
        author: &str,
        changes: &[EntityChangeRow],
    ) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;

        conn.execute(
            "INSERT OR IGNORE INTO entity_commits (hash, committed_at, author) VALUES (?1, ?2, ?3)",
            params![hash, committed_at, author],
        )?;

        if conn.changes() == 0 {
            tx.commit()?;
            return Ok(());
        }

        let pair_eligible = if changes.len() > MAX_PAIR_ENTITIES {
            0
        } else {
            1
        };

        let mut stmt = conn.prepare_cached(
            "INSERT INTO entity_changes \
             (commit_hash, qualified_name, kind, file_path, change_type, \
              old_qualified_name, old_file_path, pair_eligible) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for c in changes {
            stmt.execute(params![
                hash,
                c.qualified_name,
                c.kind,
                c.file_path,
                c.change_type,
                c.old_qualified_name,
                c.old_file_path,
                pair_eligible,
            ])?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn entity_cochange_for_symbol(
        &self,
        qualified_name: &str,
        threshold: f64,
    ) -> Result<Vec<(String, String, f64, f64, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "WITH target_names AS (
                SELECT ?1 AS name
                UNION
                SELECT ec.old_qualified_name
                FROM entity_changes ec
                WHERE ec.qualified_name = ?1 AND ec.old_qualified_name IS NOT NULL
                UNION
                SELECT ec.qualified_name
                FROM entity_changes ec
                WHERE ec.old_qualified_name = ?1
            ),
            target_commits AS (
                SELECT DISTINCT ec.commit_hash
                FROM entity_changes ec
                JOIN target_names tn ON ec.qualified_name = tn.name
                WHERE ec.pair_eligible = 1
            ),
            target_count AS (
                SELECT COUNT(*) AS cnt FROM target_commits
            ),
            cochanged AS (
                SELECT ec.qualified_name, ec.file_path, COUNT(DISTINCT ec.commit_hash) AS shared_cnt
                FROM entity_changes ec
                JOIN target_commits tc ON ec.commit_hash = tc.commit_hash
                WHERE ec.pair_eligible = 1
                  AND ec.qualified_name NOT IN (SELECT name FROM target_names)
                GROUP BY ec.qualified_name, ec.file_path
            ),
            entity_counts AS (
                SELECT qualified_name, file_path, COUNT(DISTINCT commit_hash) AS cnt
                FROM entity_changes
                WHERE pair_eligible = 1
                GROUP BY qualified_name, file_path
            )
            SELECT c.qualified_name, c.file_path, c.shared_cnt,
                   CAST(c.shared_cnt AS REAL) / (tc.cnt + fc.cnt - c.shared_cnt) AS jaccard,
                   CAST(c.shared_cnt AS REAL) / MIN(tc.cnt, fc.cnt) AS confidence
            FROM cochanged c
            JOIN target_count tc
            JOIN entity_counts fc ON fc.qualified_name = c.qualified_name
                                 AND fc.file_path = c.file_path
            WHERE CAST(c.shared_cnt AS REAL) / (tc.cnt + fc.cnt - c.shared_cnt) >= ?2
              AND c.shared_cnt >= 2
            ORDER BY jaccard DESC",
        )?;
        let rows: rusqlite::Result<Vec<(String, String, f64, f64, i64)>> = stmt
            .query_map(params![qualified_name, threshold], |row| {
                let name: String = row.get(0)?;
                let file: String = row.get(1)?;
                let shared: i64 = row.get(2)?;
                let jaccard: f64 = row.get(3)?;
                let confidence: f64 = row.get(4)?;
                Ok((name, file, jaccard, confidence, shared))
            })?
            .collect();
        Ok(rows?)
    }

    pub fn entity_change_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM entity_changes", [], |r| r.get(0))?)
    }

    pub fn entity_commit_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM entity_commits", [], |r| r.get(0))?)
    }
}
