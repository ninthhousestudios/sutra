use std::sync::Arc;

use rusqlite::params;

use super::Db;
use crate::error::Result;
use crate::rules::Severity;

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

// --- Instance acks (report-only, count-aware, content-keyed; sutra/305) ---

/// A report-only acknowledgment of one or more byte-identical forbidden_pattern
/// match instances. Keyed by the guard's content fingerprint
/// `(constraint_id, enclosing_symbol, snippet)` plus `file_path`; `accepted_count`
/// is how many matches of that key are examined-and-accepted, so surplus siblings
/// (and any future clone) still surface on the report.
#[derive(Debug, Clone)]
pub struct ConstraintInstanceAckRow {
    pub id: i64,
    pub constraint_id: Arc<str>,
    pub constraint_name: Option<Arc<str>>,
    pub file_path: String,
    pub enclosing_symbol: Option<String>,
    pub snippet: Option<String>,
    pub accepted_count: i64,
    pub rationale: Option<String>,
    pub acked_by: String,
    pub created_at: String,
    pub updated_at: String,
}

fn map_ack_row(row: &rusqlite::Row) -> rusqlite::Result<ConstraintInstanceAckRow> {
    Ok(ConstraintInstanceAckRow {
        id: row.get(0)?,
        constraint_id: Arc::from(row.get::<_, String>(1)?),
        constraint_name: row.get::<_, Option<String>>(2)?.map(Arc::from),
        file_path: row.get(3)?,
        enclosing_symbol: row.get(4)?,
        snippet: row.get(5)?,
        accepted_count: row.get(6)?,
        rationale: row.get(7)?,
        acked_by: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

const ACK_SELECT_COLS: &str = "id, constraint_id, constraint_name, file_path, \
                               enclosing_symbol, snippet, accepted_count, rationale, \
                               acked_by, created_at, updated_at";

// --- Portable acceptance projection input (.sutra/accepted.toml; sutra/303) ---

/// A waiver resolved from the tracked file, ready to insert into the cache. Owns
/// its columns (they come from a parsed file plus a name->id resolution); no
/// `id`/timestamps — the DB mints those, the file carries neither.
#[derive(Debug, Clone)]
pub struct WaiverProjection {
    pub constraint_id: String,
    pub constraint_name: Option<String>,
    pub file_path: String,
    pub symbol_qualified_name: Option<String>,
    pub rationale: String,
    pub waived_by: String,
}

/// An instance ack resolved from the tracked file, ready to insert into the cache.
#[derive(Debug, Clone)]
pub struct AckProjection {
    pub constraint_id: String,
    pub constraint_name: Option<String>,
    pub file_path: String,
    pub enclosing_symbol: Option<String>,
    pub snippet: Option<String>,
    pub accepted_count: i64,
    pub rationale: Option<String>,
    pub acked_by: String,
}

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
    Severity::from_str_lossy(s)
        .map(|s| s.ordinal())
        .unwrap_or(0)
}

pub(crate) fn active_ratchets_from_conn(conn: &rusqlite::Connection) -> Vec<ConstraintRatchetRow> {
    let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT {RATCHET_SELECT_COLS} FROM constraint_ratchets \
         WHERE released_at IS NULL ORDER BY registered_at"
    )) else {
        return Vec::new();
    };
    stmt.query_map([], map_ratchet_row)
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
}

/// Read the accepted-cache freshness marker from a read-only connection — the
/// guard's edit-time path holds a bare `Connection`, not a writable `Db`, so it
/// cannot call [`Db::get_accepted_sync_marker`]. `None` (never projected, or the
/// row/query failed) reads as stale, which is the conservative default: the
/// guard then derives waivers straight from the file rather than trusting a cache
/// it cannot confirm.
pub(crate) fn accepted_sync_marker_from_conn(conn: &rusqlite::Connection) -> Option<String> {
    conn.query_row(
        "SELECT file_hash FROM accepted_sync WHERE id = 1",
        [],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
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
    // Instance acks (report-only; sutra/305)
    // -----------------------------------------------------------------------

    /// Upsert an instance ack, setting `accepted_count` to the caller-computed
    /// value (the caller decides whether that is a fresh snapshot count or an
    /// incremented single-ack count, and caps it at what actually matches on
    /// disk). Keyed by `(constraint_id, file_path, enclosing_symbol, snippet)`.
    #[allow(clippy::too_many_arguments)]
    pub fn create_constraint_instance_ack(
        &self,
        constraint_id: &str,
        constraint_name: Option<&str>,
        file_path: &str,
        enclosing_symbol: Option<&str>,
        snippet: Option<&str>,
        accepted_count: i64,
        rationale: Option<&str>,
        acked_by: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO constraint_instance_acks
             (constraint_id, constraint_name, file_path, enclosing_symbol, snippet,
              accepted_count, rationale, acked_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(constraint_id, file_path,
                         COALESCE(enclosing_symbol, ''), COALESCE(snippet, ''))
             DO UPDATE SET
                 constraint_name = ?2,
                 accepted_count = ?6,
                 rationale = COALESCE(?7, rationale),
                 acked_by = ?8,
                 updated_at = datetime('now')",
            params![
                constraint_id,
                constraint_name,
                file_path,
                enclosing_symbol,
                snippet,
                accepted_count,
                rationale,
                acked_by,
            ],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM constraint_instance_acks \
             WHERE constraint_id = ?1 AND file_path = ?2 \
             AND COALESCE(enclosing_symbol, '') = COALESCE(?3, '') \
             AND COALESCE(snippet, '') = COALESCE(?4, '')",
            params![constraint_id, file_path, enclosing_symbol, snippet],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn get_constraint_instance_acks_for_file(
        &self,
        file_path: &str,
    ) -> Result<Vec<ConstraintInstanceAckRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ACK_SELECT_COLS} FROM constraint_instance_acks \
             WHERE file_path = ?1 ORDER BY created_at DESC"
        ))?;
        let rows = stmt
            .query_map(params![file_path], map_ack_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn get_all_constraint_instance_acks(&self) -> Result<Vec<ConstraintInstanceAckRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ACK_SELECT_COLS} FROM constraint_instance_acks ORDER BY created_at DESC"
        ))?;
        let rows = stmt
            .query_map([], map_ack_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_constraint_instance_ack(&self, ack_id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let count = conn.execute(
            "DELETE FROM constraint_instance_acks WHERE id = ?1",
            params![ack_id],
        )?;
        Ok(count > 0)
    }

    /// Instance acks whose constraint no longer exists in the loaded rules.
    /// Mirrors [`Db::reconcile_orphaned_constraint_waivers`]: an ack for a removed
    /// constraint can never suppress anything (its key matches no live finding),
    /// so it is dead state to be surfaced and GC'd. Empty `active_ids` returns all
    /// rows (every ack is orphaned).
    pub fn reconcile_orphaned_constraint_instance_acks(
        &self,
        active_ids: &[&str],
    ) -> Result<Vec<ConstraintInstanceAckRow>> {
        let conn = self.conn.lock();
        if active_ids.is_empty() {
            let mut stmt = conn.prepare(&format!(
                "SELECT {ACK_SELECT_COLS} FROM constraint_instance_acks ORDER BY created_at DESC"
            ))?;
            let rows = stmt
                .query_map([], map_ack_row)?
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
            "SELECT {ACK_SELECT_COLS} FROM constraint_instance_acks \
             WHERE constraint_id NOT IN ({placeholders}) ORDER BY created_at DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = active_ids
            .iter()
            .map(|s| s as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt
            .query_map(params.as_slice(), map_ack_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // -----------------------------------------------------------------------
    // Portable acceptance projection (.sutra/accepted.toml; sutra/303)
    // -----------------------------------------------------------------------

    /// Atomically rebuild the waiver + ack cache tables from the tracked file and
    /// stamp the freshness marker with `file_hash`. The whole reprojection is one
    /// transaction — a half-applied cache would silently under- or over-suppress
    /// findings (lesson 019ed6cd: every multi-statement SQLite mutation runs in a
    /// transaction). `file_hash` is what a later reader compares against to decide
    /// the cache is fresh, so it is committed in the same transaction as the rows
    /// it describes.
    pub fn reproject_accepted(
        &self,
        waivers: &[WaiverProjection],
        acks: &[AckProjection],
        file_hash: &str,
    ) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            conn.execute("DELETE FROM constraint_waivers", [])?;
            conn.execute("DELETE FROM constraint_instance_acks", [])?;
            let mut w_stmt = conn.prepare_cached(
                "INSERT INTO constraint_waivers
                 (constraint_id, constraint_name, file_path, symbol_qualified_name,
                  rationale, waived_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for w in waivers {
                w_stmt.execute(params![
                    w.constraint_id,
                    w.constraint_name,
                    w.file_path,
                    w.symbol_qualified_name,
                    w.rationale,
                    w.waived_by,
                ])?;
            }
            let mut a_stmt = conn.prepare_cached(
                "INSERT INTO constraint_instance_acks
                 (constraint_id, constraint_name, file_path, enclosing_symbol, snippet,
                  accepted_count, rationale, acked_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for a in acks {
                a_stmt.execute(params![
                    a.constraint_id,
                    a.constraint_name,
                    a.file_path,
                    a.enclosing_symbol,
                    a.snippet,
                    a.accepted_count,
                    a.rationale,
                    a.acked_by,
                ])?;
            }
            conn.execute(
                "UPDATE accepted_sync SET file_hash = ?1 WHERE id = 1",
                params![file_hash],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The content hash the cache was last projected from. `Ok(None)` means the
    /// cache has never been projected (always treat as stale) — distinct from a
    /// lookup failure, which propagates.
    pub fn get_accepted_sync_marker(&self) -> Result<Option<String>> {
        let conn = self.conn.lock();
        let hash: Option<String> = conn.query_row(
            "SELECT file_hash FROM accepted_sync WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        Ok(hash)
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
                 severity_floor = ?4,
                 released_at = NULL,
                 released_by = NULL,
                 release_rationale = NULL",
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
        Ok(active_ratchets_from_conn(&conn))
    }

    pub fn get_all_constraint_ratchets(&self) -> Result<Vec<ConstraintRatchetRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {RATCHET_SELECT_COLS} FROM constraint_ratchets \
             ORDER BY registered_at"
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
