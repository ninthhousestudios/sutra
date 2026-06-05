use std::collections::HashMap;

use crate::conventions::{Convention, SymbolAttrs, DRIFT_WINDOW};
use crate::db::Db;
use crate::error::Result;
use crate::similarity::hrr::HrrVec;

use super::{BiomarkerKind, HealthFinding, HealthSeverity};

pub const FCA_DRIFT_THRESHOLD: f64 = 0.10;
pub const HRR_DRIFT_THRESHOLD: f64 = 0.10;
const MIN_COHERENCE_SYMBOLS: usize = 3;

pub fn compute_fca_conformance(
    conventions: &[Convention],
    components: &[(String, String, Vec<SymbolAttrs>)],
) -> HashMap<String, f64> {
    let mut result = HashMap::new();
    for (comp_id, _comp_name, symbols) in components {
        let mut total_matched = 0usize;
        let mut total_conforming = 0usize;

        for conv in conventions {
            if let Some(ref cid) = conv.component_id {
                if cid != comp_id {
                    continue;
                }
            }

            for sym in symbols {
                let has_antecedent = conv
                    .antecedent
                    .iter()
                    .all(|a| sym.attributes.contains(a));
                if !has_antecedent {
                    continue;
                }
                total_matched += 1;
                let missing = conv
                    .consequent
                    .iter()
                    .any(|c| !sym.attributes.contains(c));
                if !missing {
                    total_conforming += 1;
                }
            }
        }

        if total_matched > 0 {
            result.insert(comp_id.clone(), total_conforming as f64 / total_matched as f64);
        }
    }
    result
}

pub fn compute_hrr_coherence(db: &Db) -> Result<HashMap<String, f64>> {
    let raw = db.strip_vectors_by_component()?;

    let mut by_component: HashMap<String, Vec<HrrVec>> = HashMap::new();
    for (comp_id, _sym_id, bytes) in &raw {
        let vec = HrrVec::from_bytes(bytes);
        by_component.entry(comp_id.clone()).or_default().push(vec);
    }

    let mut result = HashMap::new();
    for (comp_id, vectors) in &by_component {
        if vectors.len() < MIN_COHERENCE_SYMBOLS {
            continue;
        }
        let n = vectors.len();
        let mut sum = 0.0;
        let mut count = 0usize;
        for i in 0..n {
            for j in (i + 1)..n {
                sum += vectors[i].cosine_similarity(&vectors[j]);
                count += 1;
            }
        }
        if count > 0 {
            result.insert(comp_id.clone(), sum / count as f64);
        }
    }
    Ok(result)
}

pub fn detect_convention_drift(
    db: &Db,
    conformance: &HashMap<String, f64>,
    coherence: &HashMap<String, f64>,
    components: &[(String, String, Vec<SymbolAttrs>)],
    file_to_component: &HashMap<String, String>,
    id_map: &HashMap<&str, i64>,
) -> Result<Vec<HealthFinding>> {
    let comp_to_file: HashMap<&str, i64> = file_to_component
        .iter()
        .filter_map(|(path, comp_id)| {
            id_map.get(path.as_str()).map(|&fid| (comp_id.as_str(), fid))
        })
        .collect();

    let mut findings = Vec::new();
    for (comp_id, comp_name, _symbols) in components {
        let file_id = match comp_to_file.get(comp_id.as_str()) {
            Some(&fid) => fid,
            None => continue,
        };

        let snapshots = db.recent_convention_snapshots(comp_id, DRIFT_WINDOW)?;
        if snapshots.len() < DRIFT_WINDOW - 1 {
            continue;
        }

        if let Some(&current_conf) = conformance.get(comp_id) {
            if let Some(finding) = check_metric_drop(
                &snapshots,
                |s| s.fca_conformance,
                current_conf,
                FCA_DRIFT_THRESHOLD,
                file_id,
                comp_name,
                "fca_conformance",
                "convention conformance",
            ) {
                findings.push(finding);
            }
        }

        if let Some(&current_coh) = coherence.get(comp_id) {
            if let Some(finding) = check_metric_drop(
                &snapshots,
                |s| s.hrr_coherence,
                current_coh,
                HRR_DRIFT_THRESHOLD,
                file_id,
                comp_name,
                "hrr_coherence",
                "structural coherence",
            ) {
                findings.push(finding);
            }
        }
    }
    Ok(findings)
}

fn check_metric_drop(
    snapshots: &[crate::db::ConventionSnapshotRow],
    extract: impl Fn(&crate::db::ConventionSnapshotRow) -> Option<f64>,
    current: f64,
    threshold: f64,
    file_id: i64,
    comp_name: &str,
    provenance_suffix: &str,
    label: &str,
) -> Option<HealthFinding> {
    let historical: Vec<f64> = snapshots[..DRIFT_WINDOW - 1]
        .iter()
        .filter_map(|s| extract(s))
        .collect();
    if historical.len() < DRIFT_WINDOW - 1 {
        return None;
    }

    let oldest = historical[historical.len() - 1]; // snapshots are DESC, last = oldest
    let delta = oldest - current;
    if delta <= threshold {
        return None;
    }

    // Check monotonically non-increasing: oldest..current (ASC order)
    let mut series: Vec<f64> = historical.iter().rev().copied().collect();
    series.push(current);
    let monotonic = series.windows(2).all(|w| w[1] <= w[0]);
    if !monotonic {
        return None;
    }

    Some(HealthFinding {
        file_id,
        symbol_id: None,
        biomarker_kind: BiomarkerKind::ConventionDrift,
        severity: HealthSeverity::Informational,
        confidence: 1.0,
        provenance: format!("review:{provenance_suffix}"),
        metric_value: delta,
        threshold,
        detail: format!(
            "{comp_name}: {label} dropped from {oldest:.2} to {current:.2}"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_conv(
        antecedent: &[&str],
        consequent: &[&str],
        component_id: Option<&str>,
    ) -> Convention {
        Convention {
            id: "test".into(),
            antecedent: antecedent.iter().map(|s| s.to_string()).collect(),
            consequent: consequent.iter().map(|s| s.to_string()).collect(),
            support: 3,
            confidence: 1.0,
            component_id: component_id.map(|s| s.into()),
        }
    }

    fn make_sym(name: &str, file: &str, attrs: &[&str], comp: Option<&str>) -> SymbolAttrs {
        SymbolAttrs {
            name: name.into(),
            file: file.into(),
            attributes: attrs.iter().map(|s| s.to_string()).collect(),
            component_id: comp.map(|s| s.into()),
        }
    }

    #[test]
    fn fca_conformance_basic() {
        let convs = vec![make_conv(&["kind:function", "vis:pub"], &["has_doc"], None)];
        let syms = vec![
            make_sym("a", "f.rs", &["kind:function", "vis:pub", "has_doc"], Some("c1")),
            make_sym("b", "f.rs", &["kind:function", "vis:pub", "has_doc"], Some("c1")),
            make_sym("c", "f.rs", &["kind:function", "vis:pub"], Some("c1")), // missing has_doc
        ];
        let components = vec![("c1".into(), "comp1".into(), syms)];
        let result = compute_fca_conformance(&convs, &components);
        let conf = result.get("c1").unwrap();
        assert!((conf - 2.0 / 3.0).abs() < 1e-9, "expected ~0.667, got {conf}");
    }

    #[test]
    fn fca_conformance_no_matches() {
        let convs = vec![make_conv(&["kind:struct"], &["has_doc"], None)];
        let syms = vec![
            make_sym("a", "f.rs", &["kind:function", "vis:pub"], Some("c1")),
        ];
        let components = vec![("c1".into(), "comp1".into(), syms)];
        let result = compute_fca_conformance(&convs, &components);
        assert!(result.get("c1").is_none());
    }

    #[test]
    fn fca_conformance_component_scoped() {
        let convs = vec![
            make_conv(&["kind:function"], &["has_doc"], Some("c1")),
            make_conv(&["kind:function"], &["vis:pub"], Some("c2")),
        ];
        let syms_c1 = vec![
            make_sym("a", "a.rs", &["kind:function", "has_doc"], Some("c1")),
            make_sym("b", "a.rs", &["kind:function"], Some("c1")), // missing has_doc
        ];
        let syms_c2 = vec![
            make_sym("x", "x.rs", &["kind:function", "vis:pub"], Some("c2")),
        ];
        let components = vec![
            ("c1".into(), "comp1".into(), syms_c1),
            ("c2".into(), "comp2".into(), syms_c2),
        ];
        let result = compute_fca_conformance(&convs, &components);
        assert!((result["c1"] - 0.5).abs() < 1e-9);
        assert!((result["c2"] - 1.0).abs() < 1e-9);
    }
}
