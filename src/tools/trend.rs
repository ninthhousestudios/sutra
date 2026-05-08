use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, SnapshotRow};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrendArgs {
    pub workspace: String,
    /// ISO timestamp for the start of the comparison window.
    /// Defaults to the second-most-recent snapshot.
    #[serde(default)]
    pub from: Option<String>,
    /// ISO timestamp for the end of the comparison window.
    /// Defaults to the most recent snapshot.
    #[serde(default)]
    pub to: Option<String>,
}
use crate::error::Result;

pub fn handle(db: &Db, from: Option<&str>, to: Option<&str>) -> Result<serde_json::Value> {
    let (snap_from, snap_to) = resolve_snapshots(db, from, to)?;

    let deltas = json!({
        "files_parsed": snap_to.files_parsed - snap_from.files_parsed,
        "symbols_extracted": snap_to.symbols_extracted - snap_from.symbols_extracted,
        "refs_extracted": snap_to.refs_extracted - snap_from.refs_extracted,
        "parse_errors": snap_to.parse_errors - snap_from.parse_errors,
        "duration_ms": snap_to.duration_ms - snap_from.duration_ms,
        "total_complexity": snap_to.total_complexity - snap_from.total_complexity,
        "dead_symbol_count": snap_to.dead_symbol_count - snap_from.dead_symbol_count,
        "hotspot_count": snap_to.hotspot_count - snap_from.hotspot_count,
        "health_score": snap_to.health_score - snap_from.health_score,
    });

    Ok(json!({
        "from": snapshot_to_json(&snap_from),
        "to": snapshot_to_json(&snap_to),
        "deltas": deltas,
    }))
}

fn resolve_snapshots(
    db: &Db,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<(SnapshotRow, SnapshotRow)> {
    match (from, to) {
        (Some(f), Some(t)) => {
            let range = db.snapshots_between(f, t)?;
            if range.len() < 2 {
                return Err(crate::error::SutraError::Internal(
                    "need at least 2 snapshots in the given range".into(),
                ));
            }
            Ok((range[0].clone(), range[range.len() - 1].clone()))
        }
        _ => {
            let snaps = db.latest_snapshots(2)?;
            if snaps.len() < 2 {
                return Err(crate::error::SutraError::Internal(
                    "need at least 2 snapshots to compute trend".into(),
                ));
            }
            // latest_snapshots returns newest-first
            Ok((snaps[1].clone(), snaps[0].clone()))
        }
    }
}

fn snapshot_to_json(s: &SnapshotRow) -> serde_json::Value {
    json!({
        "id": s.id,
        "timestamp": s.timestamp,
        "files_parsed": s.files_parsed,
        "symbols_extracted": s.symbols_extracted,
        "refs_extracted": s.refs_extracted,
        "parse_errors": s.parse_errors,
        "duration_ms": s.duration_ms,
        "total_complexity": s.total_complexity,
        "dead_symbol_count": s.dead_symbol_count,
        "hotspot_count": s.hotspot_count,
        "health_score": s.health_score,
    })
}
