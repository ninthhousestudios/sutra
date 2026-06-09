use rusqlite::params;

use super::Db;
use crate::error::Result;

#[derive(Debug, Clone)]
pub struct HealthFindingRow {
    pub id: i64,
    pub file_id: i64,
    pub symbol_id: Option<i64>,
    pub biomarker_kind: String,
    pub severity: String,
    pub confidence: f64,
    pub provenance: String,
    pub metric_value: f64,
    pub threshold: f64,
    pub detail: String,
}

#[derive(Debug, Clone)]
pub struct HealthWaiverRow {
    pub id: i64,
    pub biomarker_kind: String,
    pub file_path: String,
    pub symbol_qualified_name: Option<String>,
    pub rationale: String,
    pub waived_by: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug)]
pub struct NestingExceedRow {
    pub file_id: i64,
    pub symbol_id: i64,
    pub qualified_name: String,
    pub max_nesting: i64,
}

fn map_finding_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HealthFindingRow> {
    Ok(HealthFindingRow {
        id: row.get(0)?,
        file_id: row.get(1)?,
        symbol_id: row.get(2)?,
        biomarker_kind: row.get(3)?,
        severity: row.get(4)?,
        confidence: row.get(5)?,
        provenance: row.get(6)?,
        metric_value: row.get(7)?,
        threshold: row.get(8)?,
        detail: row.get(9)?,
    })
}

const FINDING_SELECT_COLS: &str = "id, file_id, symbol_id, biomarker_kind, severity, confidence, provenance, \
     metric_value, threshold, detail";

fn map_waiver_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HealthWaiverRow> {
    Ok(HealthWaiverRow {
        id: row.get(0)?,
        biomarker_kind: row.get(1)?,
        file_path: row.get(2)?,
        symbol_qualified_name: row.get(3)?,
        rationale: row.get(4)?,
        waived_by: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

const WAIVER_SELECT_COLS: &str = "id, biomarker_kind, file_path, symbol_qualified_name, rationale, waived_by, \
     created_at, updated_at";

impl Db {
    pub fn symbols_exceeding_nesting(&self, threshold: i64) -> Result<Vec<NestingExceedRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT file_id, id, qualified_name, max_nesting FROM symbols
             WHERE max_nesting IS NOT NULL AND max_nesting > ?1",
        )?;
        let rows = stmt
            .query_map(params![threshold], |row| {
                Ok(NestingExceedRow {
                    file_id: row.get(0)?,
                    symbol_id: row.get(1)?,
                    qualified_name: row.get(2)?,
                    max_nesting: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn replace_health_findings(&self, findings: &[crate::health::HealthFinding]) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        {
            conn.execute("DELETE FROM health_findings", [])?;
            let mut stmt = conn.prepare(
                "INSERT INTO health_findings
                 (file_id, symbol_id, biomarker_kind, severity, confidence, provenance,
                  metric_value, threshold, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for f in findings {
                stmt.execute(params![
                    f.file_id,
                    f.symbol_id,
                    f.biomarker_kind.as_str(),
                    f.severity.as_str(),
                    f.confidence,
                    f.provenance,
                    f.metric_value,
                    f.threshold,
                    f.detail,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_health_findings(
        &self,
        file_id: Option<i64>,
        biomarker_kind: Option<&str>,
    ) -> Result<Vec<HealthFindingRow>> {
        let conn = self.conn.lock();
        let (sql, needs_file, needs_kind) = match (file_id.is_some(), biomarker_kind.is_some()) {
            (true, true) => (
                format!(
                    "SELECT {FINDING_SELECT_COLS} FROM health_findings
                     WHERE file_id = ?1 AND biomarker_kind = ?2"
                ),
                true,
                true,
            ),
            (true, false) => (
                format!("SELECT {FINDING_SELECT_COLS} FROM health_findings WHERE file_id = ?1"),
                true,
                false,
            ),
            (false, true) => (
                format!(
                    "SELECT {FINDING_SELECT_COLS} FROM health_findings WHERE biomarker_kind = ?1"
                ),
                false,
                true,
            ),
            (false, false) => (
                format!("SELECT {FINDING_SELECT_COLS} FROM health_findings"),
                false,
                false,
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = match (needs_file, needs_kind) {
            (true, true) => stmt
                .query_map(
                    params![file_id.unwrap(), biomarker_kind.unwrap()],
                    map_finding_row,
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            (true, false) => stmt
                .query_map(params![file_id.unwrap()], map_finding_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            (false, true) => stmt
                .query_map(params![biomarker_kind.unwrap()], map_finding_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            (false, false) => stmt
                .query_map([], map_finding_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    pub fn get_health_waivers(&self) -> Result<Vec<HealthWaiverRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("SELECT {WAIVER_SELECT_COLS} FROM health_waivers"))?;
        let rows = stmt
            .query_map([], map_waiver_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn create_health_waiver(
        &self,
        biomarker_kind: &str,
        file_path: &str,
        symbol_qualified_name: Option<&str>,
        rationale: &str,
        waived_by: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO health_waivers
             (biomarker_kind, file_path, symbol_qualified_name, rationale, waived_by)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(biomarker_kind, file_path, COALESCE(symbol_qualified_name, ''))
             DO UPDATE SET
                 rationale = ?4,
                 waived_by = ?5,
                 updated_at = datetime('now')",
            params![
                biomarker_kind,
                file_path,
                symbol_qualified_name,
                rationale,
                waived_by
            ],
        )?;
        let id: i64 = conn.query_row(
            "SELECT id FROM health_waivers
             WHERE biomarker_kind = ?1 AND file_path = ?2
             AND COALESCE(symbol_qualified_name, '') = COALESCE(?3, '')",
            params![biomarker_kind, file_path, symbol_qualified_name],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn delete_health_waiver(&self, waiver_id: i64) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM health_waivers WHERE id = ?1",
            params![waiver_id],
        )?;
        Ok(())
    }

    pub fn get_health_findings_with_waiver_status(&self) -> Result<Vec<(HealthFindingRow, bool)>> {
        let findings = self.get_health_findings(None, None)?;
        let waivers = self.get_health_waivers()?;

        let results = findings
            .into_iter()
            .map(|f| {
                let file_path = self.file_path_by_id(f.file_id);
                let symbol_name = f
                    .symbol_id
                    .and_then(|sid| self.symbol_qualified_name(sid).ok());
                let is_waived = if let Ok(ref path) = file_path {
                    waivers.iter().any(|w| {
                        w.biomarker_kind == f.biomarker_kind
                            && w.file_path == *path
                            && match &w.symbol_qualified_name {
                                None => true,
                                Some(wname) => symbol_name.as_deref() == Some(wname.as_str()),
                            }
                    })
                } else {
                    false
                };
                (f, is_waived)
            })
            .collect();
        Ok(results)
    }

    fn symbol_qualified_name(&self, symbol_id: i64) -> Result<String> {
        let conn = self.conn.lock();
        let name: String = conn.query_row(
            "SELECT qualified_name FROM symbols WHERE id = ?1",
            params![symbol_id],
            |row| row.get(0),
        )?;
        Ok(name)
    }

    fn file_path_by_id(&self, file_id: i64) -> Result<String> {
        let conn = self.conn.lock();
        let path: String = conn.query_row(
            "SELECT path FROM files WHERE id = ?1",
            params![file_id],
            |row| row.get(0),
        )?;
        Ok(path)
    }
}
