use rusqlite::params;

use super::Db;
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct ConstraintWaiverRow {
    pub id: i64,
    pub constraint_id: String,
    pub constraint_name: Option<String>,
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
        constraint_id: row.get(1)?,
        constraint_name: row.get(2)?,
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
