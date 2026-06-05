use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, SnapshotFileRow, SnapshotRow};
use crate::error::Result;

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
    /// File path for per-file historical health query.
    /// When set, returns a time series instead of a comparison.
    #[serde(default)]
    pub path: Option<String>,
    /// Max snapshots for per-file history (default 10).
    #[serde(default)]
    pub limit: Option<usize>,
}

pub fn handle(db: &Db, args: &TrendArgs) -> Result<serde_json::Value> {
    if let Some(path) = &args.path {
        return handle_history(db, path, args.limit.unwrap_or(10));
    }
    handle_comparison(db, args.from.as_deref(), args.to.as_deref())
}

fn handle_history(db: &Db, path: &str, limit: usize) -> Result<serde_json::Value> {
    let history = db.file_health_history(path, limit)?;
    let snapshots: Vec<_> = history
        .iter()
        .map(|(ts, score, cats)| {
            json!({
                "timestamp": ts,
                "health_score": round2(*score),
                "category_deductions": serde_json::from_str::<serde_json::Value>(cats)
                    .unwrap_or(json!({})),
            })
        })
        .collect();
    Ok(json!({
        "mode": "history",
        "path": path,
        "snapshots": snapshots,
        "count": snapshots.len(),
    }))
}

fn handle_comparison(
    db: &Db,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<serde_json::Value> {
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
        "health_score": round2(snap_to.health_score - snap_from.health_score),
        "pattern_family_count": snap_to.pattern_family_count - snap_from.pattern_family_count,
    });

    let from_files = db.snapshot_file_scores(snap_from.id)?;
    let to_files = db.snapshot_file_scores(snap_to.id)?;
    let file_deltas = compute_file_deltas(&from_files, &to_files);

    let from_comps = db.snapshot_component_scores(snap_from.id)?;
    let to_comps = db.snapshot_component_scores(snap_to.id)?;
    let component_deltas = compute_component_deltas(&from_comps, &to_comps);

    let category_deltas = compute_category_deltas(&from_files, &to_files);

    Ok(json!({
        "from": snapshot_to_json(&snap_from),
        "to": snapshot_to_json(&snap_to),
        "deltas": deltas,
        "files": file_deltas,
        "components": component_deltas,
        "categories": category_deltas,
    }))
}

fn compute_file_deltas(
    from: &[SnapshotFileRow],
    to: &[SnapshotFileRow],
) -> serde_json::Value {
    let from_map: HashMap<&str, f64> = from.iter().map(|f| (f.file_path.as_str(), f.score)).collect();
    let to_map: HashMap<&str, f64> = to.iter().map(|f| (f.file_path.as_str(), f.score)).collect();

    let mut improved = Vec::new();
    let mut degraded = Vec::new();

    for f in to {
        let prev = from_map.get(f.file_path.as_str()).copied().unwrap_or(10.0);
        let delta = f.score - prev;
        if delta.abs() < 0.005 {
            continue;
        }
        let entry = json!({
            "path": f.file_path,
            "from": round2(prev),
            "to": round2(f.score),
            "delta": round2(delta),
        });
        if delta > 0.0 {
            improved.push((delta, entry));
        } else {
            degraded.push((delta, entry));
        }
    }

    for f in from {
        if !to_map.contains_key(f.file_path.as_str()) {
            degraded.push((-f.score, json!({
                "path": f.file_path,
                "from": round2(f.score),
                "to": null,
                "delta": "removed",
            })));
        }
    }

    improved.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    degraded.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    json!({
        "improved": improved.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "degraded": degraded.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
    })
}

fn compute_component_deltas(
    from: &[crate::db::SnapshotComponentRow],
    to: &[crate::db::SnapshotComponentRow],
) -> Vec<serde_json::Value> {
    let from_map: HashMap<&str, f64> = from
        .iter()
        .map(|c| (c.component_id.as_str(), c.score))
        .collect();

    let mut deltas: Vec<(f64, serde_json::Value)> = to
        .iter()
        .map(|c| {
            let prev = from_map.get(c.component_id.as_str()).copied().unwrap_or(10.0);
            let delta = c.score - prev;
            (
                delta,
                json!({
                    "id": c.component_id,
                    "name": c.component_name,
                    "from": round2(prev),
                    "to": round2(c.score),
                    "delta": round2(delta),
                    "member_count": c.member_count,
                }),
            )
        })
        .collect();

    deltas.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    deltas.into_iter().map(|(_, v)| v).collect()
}

fn compute_category_deltas(
    from_files: &[SnapshotFileRow],
    to_files: &[SnapshotFileRow],
) -> serde_json::Value {
    let from_cats = aggregate_categories(from_files);
    let to_cats = aggregate_categories(to_files);

    let mut all_keys: Vec<&str> = from_cats.keys().chain(to_cats.keys()).copied().collect();
    all_keys.sort();
    all_keys.dedup();

    let mut result = serde_json::Map::new();
    for key in all_keys {
        let f = from_cats.get(key).copied().unwrap_or(0.0);
        let t = to_cats.get(key).copied().unwrap_or(0.0);
        result.insert(
            key.to_string(),
            json!({
                "from": round2(f),
                "to": round2(t),
                "delta": round2(t - f),
            }),
        );
    }
    serde_json::Value::Object(result)
}

fn aggregate_categories(files: &[SnapshotFileRow]) -> HashMap<&'static str, f64> {
    let category_names = [
        "organizational",
        "structural",
        "coupling",
        "freshness",
        "coverage",
    ];
    let mut totals = HashMap::new();
    for f in files {
        if let Ok(map) = serde_json::from_str::<HashMap<String, f64>>(&f.category_scores) {
            for (k, v) in &map {
                for &name in &category_names {
                    if k == name {
                        *totals.entry(name).or_insert(0.0) += v;
                    }
                }
            }
        }
    }
    totals
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
        "health_score": round2(s.health_score),
        "pattern_family_count": s.pattern_family_count,
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}
