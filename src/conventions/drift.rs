use std::collections::{HashMap, HashSet};

use crate::db::Db;
use crate::error::Result;

use super::engine::SymbolAttrs;

pub const DRIFT_THRESHOLD: f64 = 0.15;
pub const DRIFT_WINDOW: usize = 3;

#[derive(Debug, Clone)]
pub struct DriftAlert {
    pub component_id: String,
    pub component_name: String,
    pub entropy_old: f64,
    pub entropy_new: f64,
    pub delta: f64,
    pub diverging_attributes: Vec<DivergingAttribute>,
}

#[derive(Debug, Clone)]
pub struct DivergingAttribute {
    pub attribute: String,
    pub old_proportion: f64,
    pub new_proportion: f64,
}

pub fn shannon_entropy(distribution: &HashMap<String, f64>) -> f64 {
    let mut h = 0.0;
    for &p in distribution.values() {
        if p > 0.0 && p < 1.0 {
            h -= p * p.log2();
        }
    }
    h
}

pub fn compute_attribute_distribution(symbols: &[SymbolAttrs]) -> HashMap<String, f64> {
    if symbols.is_empty() {
        return HashMap::new();
    }
    let n = symbols.len() as f64;
    let mut counts: HashMap<String, usize> = HashMap::new();
    for sym in symbols {
        for attr in &sym.attributes {
            *counts.entry(attr.clone()).or_default() += 1;
        }
    }
    counts.into_iter().map(|(k, v)| (k, v as f64 / n)).collect()
}

pub fn find_diverging_attributes(
    old: &HashMap<String, f64>,
    new: &HashMap<String, f64>,
) -> Vec<DivergingAttribute> {
    let all_keys: HashSet<&str> = old
        .keys()
        .chain(new.keys())
        .map(|k| k.as_str())
        .collect();

    let mut result: Vec<DivergingAttribute> = all_keys
        .into_iter()
        .filter_map(|key| {
            let old_p = old.get(key).copied().unwrap_or(0.0);
            let new_p = new.get(key).copied().unwrap_or(0.0);
            if (new_p - 0.5).abs() < (old_p - 0.5).abs() {
                Some(DivergingAttribute {
                    attribute: key.to_string(),
                    old_proportion: old_p,
                    new_proportion: new_p,
                })
            } else {
                None
            }
        })
        .collect();

    result.sort_by(|a, b| {
        let a_shift = (a.new_proportion - a.old_proportion).abs();
        let b_shift = (b.new_proportion - b.old_proportion).abs();
        b_shift
            .partial_cmp(&a_shift)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    result
}

fn detect_drift(
    db: &Db,
    component_id: &str,
    component_name: &str,
    current_distribution: &HashMap<String, f64>,
    current_entropy: f64,
) -> Result<Option<DriftAlert>> {
    let snapshots = db.recent_convention_snapshots(component_id, DRIFT_WINDOW)?;
    if snapshots.len() < DRIFT_WINDOW - 1 {
        return Ok(None);
    }

    let oldest = &snapshots[DRIFT_WINDOW - 2];
    let delta = current_entropy - oldest.entropy;

    if delta <= DRIFT_THRESHOLD {
        return Ok(None);
    }

    let mut entropies: Vec<f64> = snapshots[..DRIFT_WINDOW - 1]
        .iter()
        .rev()
        .map(|s| s.entropy)
        .collect();
    entropies.push(current_entropy);

    let monotonic = entropies.windows(2).all(|w| w[1] >= w[0]);
    if !monotonic {
        return Ok(None);
    }

    let old_dist: HashMap<String, f64> =
        serde_json::from_str(&oldest.attribute_distribution).unwrap_or_default();
    let diverging = find_diverging_attributes(&old_dist, current_distribution);

    Ok(Some(DriftAlert {
        component_id: component_id.to_string(),
        component_name: component_name.to_string(),
        entropy_old: oldest.entropy,
        entropy_new: current_entropy,
        delta,
        diverging_attributes: diverging,
    }))
}

pub fn record_and_detect_drift(
    db: &Db,
    components: &[(String, String, Vec<SymbolAttrs>)],
) -> Result<Vec<DriftAlert>> {
    let mut alerts = Vec::new();
    for (comp_id, comp_name, symbols) in components {
        if db.component_lifecycle_state(comp_id)? == "sketch" {
            continue;
        }

        let distribution = compute_attribute_distribution(symbols);
        let entropy = shannon_entropy(&distribution);
        let dist_json = serde_json::to_string(&distribution).unwrap_or_default();
        let dist_hash = blake3::hash(dist_json.as_bytes()).to_hex().to_string();

        if let Some(alert) = detect_drift(db, comp_id, comp_name, &distribution, entropy)? {
            alerts.push(alert);
        }

        db.insert_convention_snapshot(
            comp_id,
            entropy,
            symbols.len() as i64,
            &dist_json,
            &dist_hash,
        )?;
    }
    Ok(alerts)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Entropy math ────────────────────────────────────────────────────

    #[test]
    fn entropy_uniform_four_attrs() {
        let dist: HashMap<String, f64> = [
            ("a".into(), 0.25),
            ("b".into(), 0.25),
            ("c".into(), 0.25),
            ("d".into(), 0.25),
        ]
        .into();
        assert!((shannon_entropy(&dist) - 2.0).abs() < 1e-10);
    }

    #[test]
    fn entropy_single_certain_attribute() {
        let dist: HashMap<String, f64> = [("a".into(), 1.0)].into();
        assert!((shannon_entropy(&dist) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn entropy_empty_distribution() {
        assert!((shannon_entropy(&HashMap::new()) - 0.0).abs() < 1e-10);
    }

    #[test]
    fn entropy_binary_equal_split() {
        let dist: HashMap<String, f64> = [("a".into(), 0.5), ("b".into(), 0.5)].into();
        assert!((shannon_entropy(&dist) - 1.0).abs() < 1e-10);
    }

    // ── Distribution computation ────────────────────────────────────────

    #[test]
    fn distribution_from_symbols() {
        let symbols = vec![
            SymbolAttrs {
                name: "a".into(),
                file: "f".into(),
                attributes: vec!["x".into(), "y".into()],
                component_id: None,
            },
            SymbolAttrs {
                name: "b".into(),
                file: "f".into(),
                attributes: vec!["x".into()],
                component_id: None,
            },
        ];
        let dist = compute_attribute_distribution(&symbols);
        assert!((dist["x"] - 1.0).abs() < 1e-10);
        assert!((dist["y"] - 0.5).abs() < 1e-10);
    }

    #[test]
    fn distribution_empty_symbols() {
        assert!(compute_attribute_distribution(&[]).is_empty());
    }

    // ── Diverging attributes ────────────────────────────────────────────

    #[test]
    fn diverging_identifies_attrs_moving_toward_half() {
        let old: HashMap<String, f64> = [
            ("a".into(), 0.9),
            ("b".into(), 0.1),
            ("c".into(), 0.5),
        ]
        .into();
        let new: HashMap<String, f64> = [
            ("a".into(), 0.7),
            ("b".into(), 0.3),
            ("c".into(), 0.5),
        ]
        .into();
        let diverging = find_diverging_attributes(&old, &new);
        let names: Vec<&str> = diverging.iter().map(|d| d.attribute.as_str()).collect();
        assert!(names.contains(&"a"));
        assert!(names.contains(&"b"));
        assert!(!names.contains(&"c"));
    }

    #[test]
    fn diverging_handles_new_attribute() {
        let old: HashMap<String, f64> = [("a".into(), 0.9)].into();
        let new: HashMap<String, f64> = [("a".into(), 0.9), ("b".into(), 0.3)].into();
        let diverging = find_diverging_attributes(&old, &new);
        let names: Vec<&str> = diverging.iter().map(|d| d.attribute.as_str()).collect();
        assert!(names.contains(&"b"));
    }

    // ── DB-backed drift detection ───────────────────────────────────────

    fn setup_db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_unchecked("test", dir.path()).unwrap();
        (dir, db)
    }

    #[test]
    fn drift_not_detected_insufficient_snapshots() {
        let (_dir, db) = setup_db();
        db.insert_component("comp1", "TestComp").unwrap();

        let dist: HashMap<String, f64> = [("a".into(), 0.5)].into();
        let result = detect_drift(&db, "comp1", "TestComp", &dist, 1.0).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn drift_not_detected_flat_entropy() {
        let (_dir, db) = setup_db();
        db.insert_component("comp1", "TestComp").unwrap();

        let dist = r#"{"a":0.8,"b":0.2}"#;
        let hash = blake3::hash(dist.as_bytes()).to_hex().to_string();
        db.insert_convention_snapshot("comp1", 0.72, 10, dist, &hash)
            .unwrap();
        db.insert_convention_snapshot("comp1", 0.72, 10, dist, &hash)
            .unwrap();

        let current_dist: HashMap<String, f64> =
            [("a".into(), 0.8), ("b".into(), 0.2)].into();
        let result = detect_drift(&db, "comp1", "TestComp", &current_dist, 0.72).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn drift_detected_increasing_entropy() {
        let (_dir, db) = setup_db();
        db.insert_component("comp1", "TestComp").unwrap();

        let dist1 = r#"{"a":0.9,"b":0.1}"#;
        let dist2 = r#"{"a":0.8,"b":0.2}"#;
        let h1 = blake3::hash(dist1.as_bytes()).to_hex().to_string();
        let h2 = blake3::hash(dist2.as_bytes()).to_hex().to_string();
        db.insert_convention_snapshot("comp1", 0.47, 10, dist1, &h1)
            .unwrap();
        db.insert_convention_snapshot("comp1", 0.55, 10, dist2, &h2)
            .unwrap();

        let current_dist: HashMap<String, f64> =
            [("a".into(), 0.65), ("b".into(), 0.35)].into();
        let current_entropy = shannon_entropy(&current_dist);
        let result =
            detect_drift(&db, "comp1", "TestComp", &current_dist, current_entropy).unwrap();
        assert!(result.is_some());
        let alert = result.unwrap();
        assert!(alert.delta > DRIFT_THRESHOLD);
        assert!(!alert.diverging_attributes.is_empty());
    }

    #[test]
    fn drift_not_detected_decreasing_entropy() {
        let (_dir, db) = setup_db();
        db.insert_component("comp1", "TestComp").unwrap();

        let dist = r#"{"a":0.5,"b":0.5}"#;
        let hash = blake3::hash(dist.as_bytes()).to_hex().to_string();
        db.insert_convention_snapshot("comp1", 1.0, 10, dist, &hash)
            .unwrap();
        db.insert_convention_snapshot("comp1", 0.9, 10, dist, &hash)
            .unwrap();

        let current_dist: HashMap<String, f64> =
            [("a".into(), 0.8), ("b".into(), 0.2)].into();
        let current_entropy = shannon_entropy(&current_dist);
        let result =
            detect_drift(&db, "comp1", "TestComp", &current_dist, current_entropy).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn drift_not_detected_non_monotonic() {
        let (_dir, db) = setup_db();
        db.insert_component("comp1", "TestComp").unwrap();

        let dist = r#"{"a":0.5,"b":0.5}"#;
        let hash = blake3::hash(dist.as_bytes()).to_hex().to_string();
        // oldest=0.5, middle=0.4 (dip), current=0.8 → net increase but not monotonic
        db.insert_convention_snapshot("comp1", 0.5, 10, dist, &hash)
            .unwrap();
        db.insert_convention_snapshot("comp1", 0.4, 10, dist, &hash)
            .unwrap();

        let current_dist: HashMap<String, f64> =
            [("a".into(), 0.6), ("b".into(), 0.4)].into();
        let result = detect_drift(&db, "comp1", "TestComp", &current_dist, 0.8).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn sketch_mode_suppresses_drift_and_recording() {
        let (_dir, db) = setup_db();
        db.insert_component("comp1", "TestComp").unwrap();
        db.set_component_lifecycle("comp1", "sketch").unwrap();

        let symbols = vec![SymbolAttrs {
            name: "a".into(),
            file: "f".into(),
            attributes: vec!["x".into()],
            component_id: Some("comp1".into()),
        }];
        let components = vec![("comp1".to_string(), "TestComp".to_string(), symbols)];
        let alerts = record_and_detect_drift(&db, &components).unwrap();
        assert!(alerts.is_empty());

        let snaps = db.recent_convention_snapshots("comp1", 10).unwrap();
        assert!(snaps.is_empty());
    }

    #[test]
    fn record_and_detect_stores_snapshot() {
        let (_dir, db) = setup_db();
        db.insert_component("comp1", "TestComp").unwrap();

        let symbols = vec![
            SymbolAttrs {
                name: "a".into(),
                file: "f".into(),
                attributes: vec!["x".into(), "y".into()],
                component_id: Some("comp1".into()),
            },
            SymbolAttrs {
                name: "b".into(),
                file: "f".into(),
                attributes: vec!["x".into()],
                component_id: Some("comp1".into()),
            },
        ];
        let components = vec![("comp1".to_string(), "TestComp".to_string(), symbols)];
        let _alerts = record_and_detect_drift(&db, &components).unwrap();

        let snaps = db.recent_convention_snapshots("comp1", 10).unwrap();
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].symbol_count, 2);
        assert!(snaps[0].entropy > 0.0);
    }
}
