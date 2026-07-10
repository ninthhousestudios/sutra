use std::sync::Arc;

use rusqlite::params;

use super::Db;
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct ConstraintWaiverRow {
    pub id: i64,
    pub constraint_id: Arc<str>,
    pub constraint_name: Option<Arc<str>>,
    pub file_path: String,
    pub symbol_qualified_name: Option<String>,
    pub rationale: String,
    pub waived_by: String,
    pub created_at: String,
    pub updated_at: String,
}

fn map_waiver_row(row: &rusqlite::Row) -> rusqlite::Result<ConstraintWaiverRow> {
    Ok(ConstraintWaiverRow {
        id: row.get(0)?,
        constraint_id: Arc::from(row.get::<_, String>(1)?),
        constraint_name: row.get::<_, Option<String>>(2)?.map(Arc::from),
        file_path: row.get(3)?,
        symbol_qualified_name: row.get(4)?,
        rationale: row.get(5)?,
        waived_by: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

const SELECT_COLS: &str = "id, constraint_id, constraint_name, file_path, \
                           symbol_qualified_name, rationale, waived_by, \
                           created_at, updated_at";

// --- Ratchet registry ---

#[derive(Debug, Clone)]
pub struct ConstraintRatchetRow {
    pub id: i64,
    pub constraint_id: Arc<str>,
    pub name: Option<Arc<str>>,
    pub rendered_description: String,
    pub severity_floor: String,
    pub registered_at: String,
    pub released_at: Option<String>,
    pub released_by: Option<String>,
    pub release_rationale: Option<String>,
}

fn map_ratchet_row(row: &rusqlite::Row) -> rusqlite::Result<ConstraintRatchetRow> {
    Ok(ConstraintRatchetRow {
        id: row.get(0)?,
        constraint_id: Arc::from(row.get::<_, String>(1)?),
        name: row.get::<_, Option<String>>(2)?.map(Arc::from),
        rendered_description: row.get(3)?,
        severity_floor: row.get(4)?,
        registered_at: row.get(5)?,
        released_at: row.get(6)?,
        released_by: row.get(7)?,
        release_rationale: row.get(8)?,
    })
}

const RATCHET_SELECT_COLS: &str = "id, constraint_id, name, rendered_description, severity_floor, \
     registered_at, released_at, released_by, release_rationale";

fn severity_ordinal(s: &str) -> u8 {
    match s {
        "blocking" => 2,
        "advisory" => 1,
        "informational" => 0,
        _ => 0,
    }
}

impl Db {
    pub fn create_constraint_waiver(
        &self,
        constraint_id: &str,
        constraint_name: Option<&str>,
        file_path: &str,
        symbol_qualified_name: Option<&str>,
        rationale: &str,
        waived_by: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO constraint_waivers
             (constraint_id, constraint_name, file_path, symbol_qualified_name,
              rationale, waived_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(constraint_id, file_path, COALESCE(symbol_qualified_name, ''))
             DO UPDATE SET
                 constraint_name = ?2,
                 rationale = ?5,
                 waived_by = ?6,
                 updated_at = datetime('now')",
            params![
                constraint_id,
                constraint_name,
                file_path,
                symbol_qualified_name,
                rationale,
                waived_by,
            ],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM constraint_waivers \
             WHERE constraint_id = ?1 AND file_path = ?2 \
             AND COALESCE(symbol_qualified_name, '') = COALESCE(?3, '')",
            params![constraint_id, file_path, symbol_qualified_name],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn get_constraint_waivers(
        &self,
        constraint_id: Option<&str>,
    ) -> Result<Vec<ConstraintWaiverRow>> {
        let conn = self.conn.lock();
        let rows = match constraint_id {
            Some(id) => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SELECT_COLS} FROM constraint_waivers \
                     WHERE constraint_id = ?1 ORDER BY created_at DESC"
                ))?;
                stmt.query_map(params![id], map_waiver_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
            None => {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {SELECT_COLS} FROM constraint_waivers \
                     ORDER BY created_at DESC"
                ))?;
                stmt.query_map([], map_waiver_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            }
        };
        Ok(rows)
    }

    pub fn get_constraint_waivers_for_file(
        &self,
        file_path: &str,
    ) -> Result<Vec<ConstraintWaiverRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SELECT_COLS} FROM constraint_waivers \
             WHERE file_path = ?1 ORDER BY created_at DESC"
        ))?;
        let rows = stmt
            .query_map(params![file_path], map_waiver_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn update_constraint_waiver(&self, waiver_id: i64, rationale: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let count = conn.execute(
            "UPDATE constraint_waivers SET rationale = ?2, updated_at = datetime('now') \
             WHERE id = ?1",
            params![waiver_id, rationale],
        )?;
        Ok(count > 0)
    }

    pub fn delete_constraint_waiver(&self, waiver_id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let count = conn.execute(
            "DELETE FROM constraint_waivers WHERE id = ?1",
            params![waiver_id],
        )?;
        Ok(count > 0)
    }

    // -----------------------------------------------------------------------
    // Ratchet registry
    // -----------------------------------------------------------------------

    pub fn upsert_constraint_ratchet(
        &self,
        constraint_id: &str,
        name: Option<&str>,
        rendered_description: &str,
        severity: &str,
    ) -> Result<()> {
        let conn = self.conn.lock();
        let existing_floor: Option<String> = conn
            .query_row(
                "SELECT severity_floor FROM constraint_ratchets \
                 WHERE constraint_id = ?1 AND released_at IS NULL",
                params![constraint_id],
                |row| row.get(0),
            )
            .ok();

        let floor = match existing_floor {
            Some(ref existing) => {
                if severity_ordinal(severity) > severity_ordinal(existing) {
                    severity
                } else {
                    existing.as_str()
                }
            }
            None => severity,
        };

        conn.execute(
            "INSERT INTO constraint_ratchets
             (constraint_id, name, rendered_description, severity_floor)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(constraint_id)
             DO UPDATE SET
                 name = ?2,
                 rendered_description = ?3,
                 severity_floor = ?4",
            params![constraint_id, name, rendered_description, floor],
        )?;
        Ok(())
    }

    pub fn get_constraint_ratchet(
        &self,
        constraint_id: &str,
    ) -> Result<Option<ConstraintRatchetRow>> {
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                &format!(
                    "SELECT {RATCHET_SELECT_COLS} FROM constraint_ratchets \
                     WHERE constraint_id = ?1"
                ),
                params![constraint_id],
                map_ratchet_row,
            )
            .ok();
        Ok(row)
    }

    pub fn get_active_constraint_ratchets(&self) -> Result<Vec<ConstraintRatchetRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RATCHET_SELECT_COLS} FROM constraint_ratchets \
             WHERE released_at IS NULL ORDER BY registered_at"
        ))?;
        let rows = stmt
            .query_map([], map_ratchet_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn release_constraint_ratchet(
        &self,
        constraint_id: &str,
        released_by: &str,
        rationale: &str,
    ) -> Result<bool> {
        let conn = self.conn.lock();
        let count = conn.execute(
            "UPDATE constraint_ratchets \
             SET released_at = datetime('now'), released_by = ?2, release_rationale = ?3 \
             WHERE constraint_id = ?1 AND released_at IS NULL",
            params![constraint_id, released_by, rationale],
        )?;
        Ok(count > 0)
    }

    pub fn reconcile_orphaned_constraint_waivers(
        &self,
        active_ids: &[&str],
    ) -> Result<Vec<ConstraintWaiverRow>> {
        let conn = self.conn.lock();
        if active_ids.is_empty() {
            let mut stmt = conn.prepare(&format!(
                "SELECT {SELECT_COLS} FROM constraint_waivers ORDER BY created_at DESC"
            ))?;
            let rows = stmt
                .query_map([], map_waiver_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            return Ok(rows);
        }
        let placeholders: String = active_ids
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {SELECT_COLS} FROM constraint_waivers \
             WHERE constraint_id NOT IN ({placeholders}) ORDER BY created_at DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = active_ids
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt
            .query_map(params.as_slice(), map_waiver_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}
