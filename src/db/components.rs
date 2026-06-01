use rusqlite::params;
use serde_json;

use crate::error::Result;

use super::{ComponentRow, Db};

impl Db {
    pub fn component_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM components", [], |r| r.get(0))?)
    }

    pub fn membership_count(&self) -> Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM component_membership", [], |r| r.get(0))?)
    }

    pub fn insert_component(&self, id: &str, name: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO components (id, name) VALUES (?1, ?2)",
            params![id, name],
        )?;
        Ok(())
    }

    pub fn delete_all_components(&self) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute_batch(
            "DELETE FROM component_membership; \
             DELETE FROM component_events; \
             DELETE FROM semantic_anchors; \
             DELETE FROM aliases; \
             DELETE FROM components",
        )?;
        Ok(())
    }

    /// Atomically insert components and their membership rows.
    pub fn batch_create_components(
        &self,
        components: &[(String, String, String)],
        membership: &[(String, i64)],
    ) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            let mut comp_stmt = conn.prepare_cached(
                "INSERT INTO components (id, name, prior_paths) VALUES (?1, ?2, ?3)",
            )?;
            for (id, name, prior_paths) in components {
                comp_stmt.execute(params![id, name, prior_paths])?;
            }
            let mut mem_stmt = conn.prepare_cached(
                "INSERT INTO component_membership (component_id, file_id) VALUES (?1, ?2)",
            )?;
            for (cid, fid) in membership {
                mem_stmt.execute(params![cid, fid])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn batch_insert_membership(&self, rows: &[(String, i64)]) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = conn.prepare_cached(
                "INSERT INTO component_membership (component_id, file_id) VALUES (?1, ?2)",
            )?;
            for (cid, fid) in rows {
                stmt.execute(params![cid, fid])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn all_components(&self) -> Result<Vec<ComponentRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, created_at, updated_at, dissolved_at, prior_paths \
             FROM components WHERE dissolved_at IS NULL ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ComponentRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    created_at: row.get(2)?,
                    updated_at: row.get(3)?,
                    dissolved_at: row.get(4)?,
                    prior_paths: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn component_file_paths(&self, component_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT f.path FROM component_membership cm \
             JOIN files f ON f.id = cm.file_id \
             WHERE cm.component_id = ?1 ORDER BY f.path",
        )?;
        let rows = stmt
            .query_map(params![component_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(rows)
    }

    pub fn active_components_with_paths(&self) -> Result<Vec<(String, String, Vec<String>)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, name, prior_paths FROM components WHERE dissolved_at IS NULL",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let json: Option<String> = row.get(2)?;
                Ok((id, name, json))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut result = Vec::new();
        for (id, name, json) in rows {
            let paths: Vec<String> = match json {
                Some(s) if !s.is_empty() => {
                    serde_json::from_str(&s).unwrap_or_default()
                }
                _ => Vec::new(),
            };
            result.push((id, name, paths));
        }
        Ok(result)
    }

    pub fn dissolve_component(&self, id: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE components SET dissolved_at = datetime('now') WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    pub fn update_component_paths(&self, id: &str, prior_paths_json: &str) -> Result<()> {
        self.conn.lock().execute(
            "UPDATE components SET prior_paths = ?2, updated_at = datetime('now') WHERE id = ?1",
            params![id, prior_paths_json],
        )?;
        Ok(())
    }

    pub fn insert_component_event(
        &self,
        component_id: &str,
        event_type: &str,
        detail: &str,
    ) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO component_events (component_id, event_type, detail) VALUES (?1, ?2, ?3)",
            params![component_id, event_type, detail],
        )?;
        Ok(())
    }

    pub fn clear_membership(&self) -> Result<()> {
        self.conn
            .lock()
            .execute_batch("DELETE FROM component_membership")?;
        Ok(())
    }

    pub fn upsert_clustering_meta(&self, edge_count: i64, file_count: i64) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO component_clustering_meta (id, edge_count, file_count, clustered_at)
             VALUES (1, ?1, ?2, datetime('now'))
             ON CONFLICT(id) DO UPDATE SET
                edge_count = excluded.edge_count,
                file_count = excluded.file_count,
                clustered_at = excluded.clustered_at",
            params![edge_count, file_count],
        )?;
        Ok(())
    }

    pub fn clustering_meta(&self) -> Result<Option<(i64, i64)>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT edge_count, file_count FROM component_clustering_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ) {
            Ok(tuple) => Ok(Some(tuple)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn clustering_meta_full(&self) -> Result<Option<(i64, i64, String)>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT edge_count, file_count, clustered_at FROM component_clustering_meta WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ) {
            Ok(tuple) => Ok(Some(tuple)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn component_events(&self, component_id: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT event_type, detail FROM component_events WHERE component_id = ?1",
        )?;
        let rows = stmt
            .query_map(params![component_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
