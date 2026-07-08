use std::collections::HashMap;

use rusqlite::params;
use serde_json;

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

#[derive(Debug, Clone)]
pub struct ConventionWaiverRow {
    pub id: i64,
    pub convention_id: String,
    pub symbol_qualified_name: String,
    pub component_id: String,
    pub rationale: String,
    pub waived_by: String,
    pub waived_at: String,
}

#[derive(Debug, Clone)]
pub struct ConventionSnapshotRow {
    pub id: i64,
    pub component_id: String,
    pub snapshot_ts: String,
    pub entropy: f64,
    pub symbol_count: i64,
    pub attribute_distribution: String,
    pub attribute_distribution_hash: String,
    pub fca_conformance: Option<f64>,
    pub hrr_coherence: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct ConventionTemplateRow {
    pub convention_id: String,
    pub template_text: String,
    pub exemplar_symbols: Vec<String>,
    pub generated_at: String,
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

    // --- Convention waivers ---

    pub fn create_waiver(
        &self,
        convention_id: &str,
        symbol_qualified_name: &str,
        component_id: &str,
        rationale: &str,
        waived_by: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO convention_waivers
             (convention_id, symbol_qualified_name, component_id, rationale, waived_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(convention_id, symbol_qualified_name, component_id) DO UPDATE SET
                 rationale = ?4,
                 waived_by = ?5,
                 waived_at = datetime('now')",
            params![
                convention_id,
                symbol_qualified_name,
                component_id,
                rationale,
                waived_by
            ],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM convention_waivers
             WHERE convention_id = ?1 AND symbol_qualified_name = ?2 AND component_id = ?3",
            params![convention_id, symbol_qualified_name, component_id],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn list_waivers(&self, convention_id: Option<&str>) -> Result<Vec<ConventionWaiverRow>> {
        let conn = self.conn.lock();
        let map_row = |row: &rusqlite::Row| {
            Ok(ConventionWaiverRow {
                id: row.get(0)?,
                convention_id: row.get(1)?,
                symbol_qualified_name: row.get(2)?,
                component_id: row.get(3)?,
                rationale: row.get(4)?,
                waived_by: row.get(5)?,
                waived_at: row.get(6)?,
            })
        };
        let rows = match convention_id {
            Some(id) => {
                let mut stmt = conn.prepare(
                    "SELECT id, convention_id, symbol_qualified_name, component_id,
                            rationale, waived_by, waived_at
                     FROM convention_waivers WHERE convention_id = ?1
                     ORDER BY waived_at DESC",
                )?;
                stmt.query_map(params![id], map_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id, convention_id, symbol_qualified_name, component_id,
                            rationale, waived_by, waived_at
                     FROM convention_waivers ORDER BY waived_at DESC",
                )?;
                stmt.query_map([], map_row)?
                    .collect::<std::result::Result<Vec<_>, _>>()?
            }
        };
        Ok(rows)
    }

    pub fn revoke_waiver(&self, waiver_id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let count = conn.execute(
            "DELETE FROM convention_waivers WHERE id = ?1",
            params![waiver_id],
        )?;
        Ok(count > 0)
    }

    pub fn waivers_for_check(
        &self,
    ) -> Result<HashMap<(String, String, String), crate::waivers::WaiverMeta>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT convention_id, symbol_qualified_name, component_id, rationale, waived_by
             FROM convention_waivers",
        )?;
        let map = stmt
            .query_map([], |row| {
                Ok((
                    (
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ),
                    crate::waivers::WaiverMeta {
                        rationale: row.get::<_, String>(3)?,
                        waived_by: row.get::<_, String>(4)?,
                    },
                ))
            })?
            .collect::<std::result::Result<HashMap<_, _>, _>>()?;
        Ok(map)
    }

    pub fn reconcile_orphaned_waivers(&self) -> Result<Vec<ConventionWaiverRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT w.id, w.convention_id, w.symbol_qualified_name, w.component_id,
                    w.rationale, w.waived_by, w.waived_at
             FROM convention_waivers w
             WHERE w.convention_id NOT IN (SELECT id FROM conventions)
                OR (
                    w.symbol_qualified_name NOT IN (SELECT qualified_name FROM symbols)
                    AND NOT EXISTS (
                        SELECT 1 FROM symbols s
                        JOIN files f ON s.file_id = f.id
                        WHERE f.path || '::' || s.qualified_name = w.symbol_qualified_name
                    )
                )",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ConventionWaiverRow {
                    id: row.get(0)?,
                    convention_id: row.get(1)?,
                    symbol_qualified_name: row.get(2)?,
                    component_id: row.get(3)?,
                    rationale: row.get(4)?,
                    waived_by: row.get(5)?,
                    waived_at: row.get(6)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // --- Stale convention cleanup ---

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

    #[allow(clippy::too_many_arguments)]
    pub fn insert_convention_snapshot(
        &self,
        component_id: &str,
        entropy: f64,
        symbol_count: i64,
        attribute_distribution: &str,
        attribute_distribution_hash: &str,
        fca_conformance: Option<f64>,
        hrr_coherence: Option<f64>,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO convention_snapshots
             (component_id, entropy, symbol_count, attribute_distribution, attribute_distribution_hash,
              fca_conformance, hrr_coherence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                component_id,
                entropy,
                symbol_count,
                attribute_distribution,
                attribute_distribution_hash,
                fca_conformance,
                hrr_coherence,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn recent_convention_snapshots(
        &self,
        component_id: &str,
        limit: usize,
    ) -> Result<Vec<ConventionSnapshotRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, component_id, snapshot_ts, entropy, symbol_count,
                    attribute_distribution, attribute_distribution_hash,
                    fca_conformance, hrr_coherence
             FROM convention_snapshots
             WHERE component_id = ?1
             ORDER BY snapshot_ts DESC, id DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![component_id, limit as i64], |row| {
                Ok(ConventionSnapshotRow {
                    id: row.get(0)?,
                    component_id: row.get(1)?,
                    snapshot_ts: row.get(2)?,
                    entropy: row.get(3)?,
                    symbol_count: row.get(4)?,
                    attribute_distribution: row.get(5)?,
                    attribute_distribution_hash: row.get(6)?,
                    fca_conformance: row.get(7)?,
                    hrr_coherence: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn upsert_convention_template(
        &self,
        convention_id: &str,
        template_text: &str,
        exemplar_symbols: &[String],
    ) -> Result<()> {
        let conn = self.conn.lock();
        let exemplars_json = serde_json::to_string(exemplar_symbols)
            .map_err(|e| crate::error::SutraError::Internal(e.to_string()))?;
        conn.execute(
            "INSERT INTO convention_templates (convention_id, template_text, exemplar_symbols, generated_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(convention_id) DO UPDATE SET
               template_text = excluded.template_text,
               exemplar_symbols = excluded.exemplar_symbols,
               generated_at = excluded.generated_at",
            params![convention_id, template_text, exemplars_json],
        )?;
        Ok(())
    }

    pub fn templates_for_conventions(
        &self,
        convention_ids: &[&str],
    ) -> Result<Vec<ConventionTemplateRow>> {
        let conn = self.conn.lock();
        if convention_ids.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders: Vec<String> = (1..=convention_ids.len())
            .map(|i| format!("?{i}"))
            .collect();
        let sql = format!(
            "SELECT convention_id, template_text, exemplar_symbols, generated_at
             FROM convention_templates WHERE convention_id IN ({})",
            placeholders.join(", ")
        );
        let mut stmt = conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = convention_ids
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt
            .query_map(params.as_slice(), |row| {
                let exemplars_json: String = row.get(2)?;
                let exemplar_symbols: Vec<String> =
                    serde_json::from_str(&exemplars_json).unwrap_or_default();
                Ok(ConventionTemplateRow {
                    convention_id: row.get(0)?,
                    template_text: row.get(1)?,
                    exemplar_symbols,
                    generated_at: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn replace_drift_alerts(
        &self,
        alerts: &[crate::conventions::drift::DriftAlert],
    ) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            conn.execute("DELETE FROM drift_alerts", [])?;
            let mut stmt = conn.prepare(
                "INSERT INTO drift_alerts
                 (component_id, component_name, entropy_old, entropy_new, delta, diverging_attributes)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for a in alerts {
                let div_json = serde_json::to_string(&a.diverging_attributes).unwrap_or_default();
                stmt.execute(params![
                    a.component_id,
                    a.component_name,
                    a.entropy_old,
                    a.entropy_new,
                    a.delta,
                    div_json,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_drift_alerts(&self) -> Result<Vec<crate::conventions::drift::DriftAlert>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT component_id, component_name, entropy_old, entropy_new, delta, diverging_attributes
             FROM drift_alerts",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let div_json: String = row.get(5)?;
                let diverging_attributes: Vec<crate::conventions::drift::DivergingAttribute> =
                    serde_json::from_str(&div_json).unwrap_or_default();
                Ok(crate::conventions::drift::DriftAlert {
                    component_id: row.get(0)?,
                    component_name: row.get(1)?,
                    entropy_old: row.get(2)?,
                    entropy_new: row.get(3)?,
                    delta: row.get(4)?,
                    diverging_attributes,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn delete_orphan_templates(&self, live_convention_ids: &[&str]) -> Result<usize> {
        let conn = self.conn.lock();
        if live_convention_ids.is_empty() {
            let count = conn.execute("DELETE FROM convention_templates", [])?;
            return Ok(count);
        }
        let placeholders: Vec<String> = (1..=live_convention_ids.len())
            .map(|i| format!("?{i}"))
            .collect();
        let sql = format!(
            "DELETE FROM convention_templates WHERE convention_id NOT IN ({})",
            placeholders.join(", ")
        );
        let params: Vec<&dyn rusqlite::types::ToSql> = live_convention_ids
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
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
