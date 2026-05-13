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
    pub suppressed: bool,
}

impl Db {
    pub fn upsert_convention(
        &self,
        id: &str,
        antecedent: &str,
        consequent: &str,
        support: i64,
        confidence: f64,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO conventions (id, antecedent, consequent, support, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                 support = ?4,
                 confidence = ?5,
                 last_seen = datetime('now')",
            params![id, antecedent, consequent, support, confidence],
        )?;
        Ok(())
    }

    pub fn all_conventions(&self) -> Result<Vec<ConventionRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, antecedent, consequent, support, confidence,
                    first_seen, last_seen, suppressed
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
                    suppressed: row.get::<_, i64>(7)? != 0,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn suppress_convention(&self, id: &str, suppressed: bool) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE conventions SET suppressed = ?1 WHERE id = ?2",
            params![suppressed as i64, id],
        )?;
        Ok(())
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
}
