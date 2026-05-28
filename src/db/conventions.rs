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
}

#[derive(Debug, Clone)]
pub struct ConventionOverrideRow {
    pub convention_id: String,
    pub lifecycle_state: String,
    pub override_reason: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ConventionWithOverride {
    pub id: String,
    pub antecedent: String,
    pub consequent: String,
    pub support: i64,
    pub confidence: f64,
    pub first_seen: String,
    pub last_seen: String,
    pub lifecycle_state: Option<String>,
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
                    first_seen, last_seen
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
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn all_conventions_merged(&self) -> Result<Vec<ConventionWithOverride>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.antecedent, c.consequent, c.support, c.confidence,
                    c.first_seen, c.last_seen, co.lifecycle_state
             FROM conventions c
             LEFT JOIN convention_overrides co ON c.id = co.convention_id
             ORDER BY c.confidence DESC, c.support DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ConventionWithOverride {
                    id: row.get(0)?,
                    antecedent: row.get(1)?,
                    consequent: row.get(2)?,
                    support: row.get(3)?,
                    confidence: row.get(4)?,
                    first_seen: row.get(5)?,
                    last_seen: row.get(6)?,
                    lifecycle_state: row.get(7)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_convention_lifecycle(
        &self,
        convention_id: &str,
        lifecycle_state: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO convention_overrides (convention_id, lifecycle_state, override_reason)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(convention_id) DO UPDATE SET
                 lifecycle_state = ?2,
                 override_reason = ?3,
                 updated_at = datetime('now')",
            params![convention_id, lifecycle_state, reason],
        )?;
        Ok(())
    }

    pub fn get_convention_override(
        &self,
        convention_id: &str,
    ) -> Result<Option<ConventionOverrideRow>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT convention_id, lifecycle_state, override_reason, created_at, updated_at
                 FROM convention_overrides WHERE convention_id = ?1",
                params![convention_id],
                |row| {
                    Ok(ConventionOverrideRow {
                        convention_id: row.get(0)?,
                        lifecycle_state: row.get(1)?,
                        override_reason: row.get(2)?,
                        created_at: row.get(3)?,
                        updated_at: row.get(4)?,
                    })
                },
            )
            .ok();
        Ok(row)
    }

    pub fn reconcile_orphaned_overrides(&self) -> Result<Vec<ConventionOverrideRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT convention_id, lifecycle_state, override_reason, created_at, updated_at
             FROM convention_overrides
             WHERE convention_id NOT IN (SELECT id FROM conventions)",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ConventionOverrideRow {
                    convention_id: row.get(0)?,
                    lifecycle_state: row.get(1)?,
                    override_reason: row.get(2)?,
                    created_at: row.get(3)?,
                    updated_at: row.get(4)?,
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
}
