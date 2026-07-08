use rusqlite::params;

use crate::error::Result;

use super::Db;

#[derive(Debug, Clone)]
pub struct ConventionRow {
    pub id: String,
    pub antecedent: String,
    pub consequent: String,
    pub support: i64,
    pub confidence: f64,
    pub first_seen: String,
    pub last_seen: String,
    pub component_id: Option<String>,
}

impl Db {
    pub fn upsert_convention(
        &self,
        id: &str,
        antecedent: &str,
        consequent: &str,
        support: i64,
        confidence: f64,
        component_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO conventions (id, antecedent, consequent, support, confidence, component_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                 support = ?4,
                 confidence = ?5,
                 component_id = ?6,
                 last_seen = datetime('now')",
            params![id, antecedent, consequent, support, confidence, component_id],
        )?;
        Ok(())
    }

    pub fn all_conventions(&self) -> Result<Vec<ConventionRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, antecedent, consequent, support, confidence,
                    first_seen, last_seen, component_id
             FROM conventions ORDER BY confidence DESC, support DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ConventionRow {
                    id: row.get(0)?,
                    antecedent: row.get(1)?,
                    consequent: row.get(2)?,
                    support: row.get(3)?,
                    confidence: row.get(4)?,
                    first_seen: row.get(5)?,
                    last_seen: row.get(6)?,
                    component_id: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn delete_stale_conventions(&self, current_ids: &[&str]) -> Result<usize> {
        if current_ids.is_empty() {
            let conn = self.conn.lock();
            let count = conn.execute("DELETE FROM conventions", [])?;
            return Ok(count);
        }
        let conn = self.conn.lock();
        let placeholders: Vec<String> = (1..=current_ids.len()).map(|i| format!("?{i}")).collect();
        let sql = format!(
            "DELETE FROM conventions WHERE id NOT IN ({})",
            placeholders.join(", ")
        );
        let params: Vec<&dyn rusqlite::types::ToSql> = current_ids
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let count = conn.execute(&sql, params.as_slice())?;
        Ok(count)
    }

    pub fn get_fca_hash(&self) -> Result<Option<[u8; 32]>> {
        let conn = self.conn.lock();
        match conn.query_row(
            "SELECT matrix_hash FROM fca_cache WHERE id = 1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        ) {
            Ok(blob) => {
                if blob.len() == 32 {
                    let mut arr = [0u8; 32];
                    arr.copy_from_slice(&blob);
                    Ok(Some(arr))
                } else {
                    Ok(None)
                }
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn set_fca_hash(&self, hash: &[u8; 32]) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO fca_cache (id, matrix_hash) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET matrix_hash = excluded.matrix_hash",
            params![hash.as_slice()],
        )?;
        Ok(())
    }

    pub fn convention_count(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM conventions", [], |row| row.get(0))?;
        Ok(count as usize)
    }
}
