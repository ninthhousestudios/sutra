use rusqlite::params;

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
        components: &[(String, String)],
        membership: &[(String, i64)],
    ) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            let mut comp_stmt =
                conn.prepare_cached("INSERT INTO components (id, name) VALUES (?1, ?2)")?;
            for (id, name) in components {
                comp_stmt.execute(params![id, name])?;
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
        let mut stmt =
            conn.prepare("SELECT id, name, created_at, updated_at FROM components ORDER BY name")?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ComponentRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    created_at: row.get(2)?,
                    updated_at: row.get(3)?,
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
}
