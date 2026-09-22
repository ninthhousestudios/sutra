use std::collections::HashMap;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::{Db, SnapshotCompleteness, SnapshotComponentRow, SnapshotFileRow, SnapshotRow};
use crate::error::Result;
use crate::health::compare::{self, IncomparableReason, SideSummary};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrendArgs {
    #[serde(default)]
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
    #[serde(default, alias = "file")]
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
        .map(|h| {
            let mut entry = completeness_json(h.completeness, &h.missing_biomarkers);
            entry.insert("timestamp".into(), json!(h.timestamp));
            entry.insert("health_score".into(), json!(round2(h.score)));
            if let Some(upper) = h.score_upper {
                entry.insert(
                    "score_bounds".into(),
                    json!({ "lower": round2(h.score), "upper": round2(upper) }),
                );
            }
            entry.insert(
                "category_deductions".into(),
                serde_json::from_str::<serde_json::Value>(&h.category_scores).unwrap_or(json!({})),
            );
            serde_json::Value::Object(entry)
        })
        .collect();
    Ok(json!({
        "mode": "history",
        "path": path,
        "snapshots": snapshots,
        "count": snapshots.len(),
    }))
}

fn handle_comparison(db: &Db, from: Option<&str>, to: Option<&str>) -> Result<serde_json::Value> {
    let (snap_from, snap_to) = resolve_snapshots(db, from, to)?;

    let from_files = db.snapshot_file_scores(snap_from.id)?;
    let to_files = db.snapshot_file_scores(snap_to.id)?;
    let aggregate_blocker = aggregate_incomparable_reason(&from_files, &to_files);
    let measured = |delta: f64| match aggregate_blocker {
        None => json!(round2(delta)),
        Some(_) => serde_json::Value::Null,
    };

    // Parse counters are exact observations; only the health aggregates are
    // measured claims and are gated on comparable file evidence.
    let deltas = json!({
        "files_parsed": snap_to.files_parsed - snap_from.files_parsed,
        "symbols_extracted": snap_to.symbols_extracted - snap_from.symbols_extracted,
        "refs_extracted": snap_to.refs_extracted - snap_from.refs_extracted,
        "parse_errors": snap_to.parse_errors - snap_from.parse_errors,
        "duration_ms": snap_to.duration_ms - snap_from.duration_ms,
        "total_complexity": snap_to.total_complexity - snap_from.total_complexity,
        "dead_symbol_count": snap_to.dead_symbol_count - snap_from.dead_symbol_count,
        "hotspot_count": snap_to.hotspot_count - snap_from.hotspot_count,
        "health_score": measured(snap_to.health_score - snap_from.health_score),
        "pattern_family_count": snap_to.pattern_family_count - snap_from.pattern_family_count,
    });

    let file_deltas = compute_file_deltas(&from_files, &to_files);

    let from_comps = db.snapshot_component_scores(snap_from.id)?;
    let to_comps = db.snapshot_component_scores(snap_to.id)?;
    let component_deltas = compute_component_deltas(&from_comps, &to_comps);

    let category_deltas = compute_category_deltas(&from_files, &to_files, measured);

    Ok(json!({
        "from": snapshot_to_json(&snap_from),
        "to": snapshot_to_json(&snap_to),
        "completeness": {
            "from": completeness_counts(&from_files),
            "to": completeness_counts(&to_files),
        },
        "deltas": deltas,
        "aggregate_comparison": {
            "measured": aggregate_blocker.is_none(),
            "reason": aggregate_blocker,
        },
        "files": file_deltas,
        "components": component_deltas,
        "categories": category_deltas,
    }))
}

/// Why the workspace/category health aggregates of two snapshots cannot be
/// compared as a measured change, or `None` when they can. An aggregate is
/// measured only when both sides carry per-file evidence, every observation on
/// both sides is complete, the file population is the same, and every file was
/// scored under the same basis on both sides — otherwise the aggregate moves with
/// missing analysis, membership or rules, not code quality
/// (health-evidence-contract.md § Comparison and scoring).
fn aggregate_incomparable_reason(
    from: &[SnapshotFileRow],
    to: &[SnapshotFileRow],
) -> Option<&'static str> {
    if from.is_empty() || to.is_empty() {
        return Some("no_file_evidence");
    }
    let all_complete = |files: &[SnapshotFileRow]| {
        files
            .iter()
            .all(|f| f.completeness == SnapshotCompleteness::Complete)
    };
    if !all_complete(from) || !all_complete(to) {
        return Some("incomplete_evidence");
    }
    if sorted_paths(from) != sorted_paths(to) {
        return Some("population_changed");
    }
    if from.iter().chain(to).any(|f| f.score_basis.is_none()) {
        return Some(IncomparableReason::UnknownBasis.as_str());
    }
    let to_basis: HashMap<&str, Option<&str>> = to
        .iter()
        .map(|f| (f.file_path.as_str(), f.score_basis.as_deref()))
        .collect();
    if from
        .iter()
        .any(|f| to_basis.get(f.file_path.as_str()) != Some(&f.score_basis.as_deref()))
    {
        return Some(IncomparableReason::BasisChanged.as_str());
    }
    None
}

fn sorted_paths(files: &[SnapshotFileRow]) -> Vec<&str> {
    let mut paths: Vec<&str> = files.iter().map(|f| f.file_path.as_str()).collect();
    paths.sort_unstable();
    paths
}

fn file_side(f: &SnapshotFileRow) -> SideSummary<'_> {
    SideSummary {
        completeness: f.completeness,
        basis: f.score_basis.as_deref(),
    }
}

/// Per-file comparison (health-evidence-contract.md § Comparison and scoring),
/// through the shared [`compare::temporal_blocker`] rule.
///
/// Only two complete observations of the same file scored under the same basis
/// yield a measured delta (`improved`/`degraded`). A new or removed file, a pair
/// where either side is partial or legacy (unknown completeness or basis), or a
/// pair whose basis changed (waiver policy, weights, applicability, versions)
/// goes to `incomparable` with both observations preserved — no fallback
/// baseline of 10.0, and no improved/degraded label on a change the evidence
/// cannot support. An incomparable pair is still reported at equal scores when
/// its completeness or basis changed, so a transition is never hidden.
fn compute_file_deltas(from: &[SnapshotFileRow], to: &[SnapshotFileRow]) -> serde_json::Value {
    let from_map: HashMap<&str, &SnapshotFileRow> =
        from.iter().map(|f| (f.file_path.as_str(), f)).collect();
    let to_map: HashMap<&str, &SnapshotFileRow> =
        to.iter().map(|f| (f.file_path.as_str(), f)).collect();

    let mut improved = Vec::new();
    let mut degraded = Vec::new();
    let mut incomparable = Vec::new();

    for f in to {
        let prev = from_map.get(f.file_path.as_str()).copied();
        let blocker = compare::temporal_blocker(prev.map(file_side), Some(file_side(f)));
        let Some(prev) = prev else {
            incomparable.push(incomparable_entry(
                &f.file_path,
                None,
                Some(f),
                IncomparableReason::NewFile.as_str(),
            ));
            continue;
        };
        let delta = f.score - prev.score;
        let score_changed = delta.abs() >= 0.005;
        match blocker {
            None => {
                if !score_changed {
                    continue;
                }
                let mut entry = json!({
                    "path": f.file_path,
                    "from": round2(prev.score),
                    "to": round2(f.score),
                    "delta": round2(delta),
                });
                entry["from_completeness"] = serde_json::Value::Object(completeness_json(
                    prev.completeness,
                    &prev.missing_biomarkers,
                ));
                entry["to_completeness"] = serde_json::Value::Object(completeness_json(
                    f.completeness,
                    &f.missing_biomarkers,
                ));
                if delta > 0.0 {
                    improved.push((delta, entry));
                } else {
                    degraded.push((delta, entry));
                }
            }
            Some(reason) => {
                let basis_changed = prev.score_basis != f.score_basis;
                if !score_changed && !completeness_differs(prev, f) && !basis_changed {
                    continue;
                }
                incomparable.push(incomparable_entry(
                    &f.file_path,
                    Some(prev),
                    Some(f),
                    reason.as_str(),
                ));
            }
        }
    }

    for f in from {
        if !to_map.contains_key(f.file_path.as_str()) {
            incomparable.push(incomparable_entry(
                &f.file_path,
                Some(f),
                None,
                IncomparableReason::RemovedFile.as_str(),
            ));
        }
    }

    improved.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    degraded.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    incomparable.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));

    json!({
        "improved": improved.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "degraded": degraded.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
        "incomparable": incomparable,
    })
}

/// An incomparable file pair: both observations (either may be absent), no
/// `delta`, and the reason a measured delta is unsupported.
fn incomparable_entry(
    path: &str,
    from: Option<&SnapshotFileRow>,
    to: Option<&SnapshotFileRow>,
    reason: &str,
) -> serde_json::Value {
    let side = |f: Option<&SnapshotFileRow>| match f {
        Some(f) => (
            json!(round2(f.score)),
            serde_json::Value::Object(completeness_json(f.completeness, &f.missing_biomarkers)),
        ),
        None => (serde_json::Value::Null, serde_json::Value::Null),
    };
    let (from_score, from_completeness) = side(from);
    let (to_score, to_completeness) = side(to);
    let (completeness_changed, basis_changed) = match (from, to) {
        (Some(a), Some(b)) => (completeness_differs(a, b), a.score_basis != b.score_basis),
        _ => (false, false),
    };
    json!({
        "path": path,
        "from": from_score,
        "to": to_score,
        "from_completeness": from_completeness,
        "to_completeness": to_completeness,
        "completeness_changed": completeness_changed,
        "basis_changed": basis_changed,
        "reason": reason,
    })
}

/// Completeness status or the *set* of missing biomarkers differs. Order-
/// insensitive: legacy rows may hold the scorer's unstable order.
fn completeness_differs(a: &SnapshotFileRow, b: &SnapshotFileRow) -> bool {
    a.completeness != b.completeness
        || sorted_names(&a.missing_biomarkers) != sorted_names(&b.missing_biomarkers)
}

fn sorted_names(names: &[String]) -> Vec<&str> {
    let mut sorted: Vec<&str> = names.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted
}

/// Shared completeness serialization for history entries and comparison sides.
/// `partial` is `null` when completeness was never recorded — a legacy default
/// cannot prove the score complete.
fn completeness_json(
    completeness: SnapshotCompleteness,
    missing_biomarkers: &[String],
) -> serde_json::Map<String, serde_json::Value> {
    let partial = match completeness {
        SnapshotCompleteness::Complete => json!(false),
        SnapshotCompleteness::Partial => json!(true),
        SnapshotCompleteness::Unknown => serde_json::Value::Null,
    };
    let mut map = serde_json::Map::new();
    map.insert("completeness".into(), json!(completeness.as_str()));
    map.insert("partial".into(), partial);
    map.insert("missing_biomarkers".into(), json!(missing_biomarkers));
    map
}

/// Per-side count of file observations by completeness, so readers of the
/// aggregate `deltas`/`categories` can see how much of each side was measured.
fn completeness_counts(files: &[SnapshotFileRow]) -> serde_json::Value {
    let count = |c: SnapshotCompleteness| files.iter().filter(|f| f.completeness == c).count();
    json!({
        "complete": count(SnapshotCompleteness::Complete),
        "partial": count(SnapshotCompleteness::Partial),
        "unknown": count(SnapshotCompleteness::Unknown),
    })
}

/// Per-component comparison. A component delta is measured only when both
/// snapshots recorded the component as complete under the same basis — which
/// covers its membership, its member files' bases and the aggregation rule.
/// New components have no fallback baseline of 10.0 and removed components are
/// listed; both, and every other unsupported pair, carry `measured: false` and
/// a `reason` with `delta: null`. Sorted: measured deltas worst-first, then the
/// incomparable entries by name.
fn component_side(c: &SnapshotComponentRow) -> SideSummary<'_> {
    SideSummary {
        completeness: c.completeness,
        basis: c.score_basis.as_deref(),
    }
}

fn compute_component_deltas(
    from: &[SnapshotComponentRow],
    to: &[SnapshotComponentRow],
) -> Vec<serde_json::Value> {
    let from_map: HashMap<&str, &SnapshotComponentRow> =
        from.iter().map(|c| (c.component_id.as_str(), c)).collect();
    let to_ids: std::collections::HashSet<&str> =
        to.iter().map(|c| c.component_id.as_str()).collect();

    let mut measured: Vec<(f64, serde_json::Value)> = Vec::new();
    let mut incomparable: Vec<serde_json::Value> = Vec::new();
    for c in to {
        let prev = from_map.get(c.component_id.as_str()).copied();
        let entry = |from: Option<f64>, delta: Option<f64>, reason: Option<&str>| {
            json!({
                "id": c.component_id,
                "name": c.component_name,
                "from": from.map(round2),
                "to": round2(c.score),
                "delta": delta.map(round2),
                "measured": reason.is_none(),
                "reason": reason,
                "from_completeness": prev.map(|p| p.completeness.as_str()),
                "to_completeness": c.completeness.as_str(),
                "member_count": c.member_count,
            })
        };
        match (
            compare::temporal_blocker(prev.map(component_side), Some(component_side(c))),
            prev,
        ) {
            (None, Some(p)) => {
                let delta = c.score - p.score;
                measured.push((delta, entry(Some(p.score), Some(delta), None)));
            }
            (reason, p) => {
                let reason = reason.unwrap_or(IncomparableReason::NewFile);
                let token = match reason {
                    IncomparableReason::NewFile => "new_component",
                    other => other.as_str(),
                };
                incomparable.push(entry(p.map(|p| p.score), None, Some(token)));
            }
        }
    }
    for p in from {
        if !to_ids.contains(p.component_id.as_str()) {
            incomparable.push(json!({
                "id": p.component_id,
                "name": p.component_name,
                "from": round2(p.score),
                "to": null,
                "delta": null,
                "measured": false,
                "reason": "removed_component",
                "from_completeness": p.completeness.as_str(),
                "to_completeness": null,
                "member_count": p.member_count,
            }));
        }
    }

    measured.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    incomparable.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    measured
        .into_iter()
        .map(|(_, v)| v)
        .chain(incomparable)
        .collect()
}

fn compute_category_deltas(
    from_files: &[SnapshotFileRow],
    to_files: &[SnapshotFileRow],
    measured: impl Fn(f64) -> serde_json::Value,
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
                "delta": measured(t - f),
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
