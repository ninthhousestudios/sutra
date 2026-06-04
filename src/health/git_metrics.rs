use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Deserialize;

use crate::db::Db;
use crate::error::Result;

use super::{BiomarkerKind, HealthFinding, HealthSeverity};

const SCATTER_PARTNER_THRESHOLD: i64 = 8;
const SCATTER_COMMIT_THRESHOLD: i64 = 3;
const ENTROPY_THRESHOLD: f64 = 3.0;
const ENTROPY_HALF_LIFE_DAYS: f64 = 180.0;
const MAX_COMMIT_WIDTH: i64 = 30;
const OWNERSHIP_TOP_THRESHOLD: f64 = 0.40;
const OWNERSHIP_MINOR_THRESHOLD: f64 = 0.05;
const OWNERSHIP_MINOR_COUNT: usize = 3;
const HIDDEN_COUPLING_MIN: f64 = 0.50;
const HIDDEN_COUPLING_HIGH: f64 = 0.65;

#[derive(Debug, Default, Deserialize)]
pub struct OwnersConfig {
    #[serde(default)]
    pub aliases: HashMap<String, String>,
}

pub fn load_owners_config(workspace_root: &Path) -> OwnersConfig {
    let path = workspace_root.join(".sutra/owners.toml");
    match std::fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_default(),
        Err(_) => OwnersConfig::default(),
    }
}

fn file_path_map(db: &Db) -> Result<HashMap<i64, String>> {
    Ok(db.all_files()?.into_iter().map(|f| (f.id, f.path)).collect())
}

pub fn compute_co_change_scatter(db: &Db) -> Result<Vec<HealthFinding>> {
    let partners = db.file_cochange_partners()?;
    let paths = file_path_map(db)?;
    let findings = partners
        .into_iter()
        .filter(|&(_, partner_count, commit_count)| {
            partner_count >= SCATTER_PARTNER_THRESHOLD && commit_count >= SCATTER_COMMIT_THRESHOLD
        })
        .map(|(file_id, partner_count, commit_count)| {
            let path = paths.get(&file_id).map(|s| s.as_str()).unwrap_or("?");
            HealthFinding {
                file_id,
                symbol_id: None,
                biomarker_kind: BiomarkerKind::CoChangeScatter,
                severity: BiomarkerKind::CoChangeScatter.default_severity(),
                confidence: 1.0,
                provenance: "computed".into(),
                metric_value: partner_count as f64,
                threshold: SCATTER_PARTNER_THRESHOLD as f64,
                detail: format!(
                    "{path} has {partner_count} co-change partners across {commit_count} commits"
                ),
            }
        })
        .collect();
    Ok(findings)
}

pub fn compute_change_entropy(db: &Db) -> Result<Vec<HealthFinding>> {
    let data = db.file_commit_sizes(MAX_COMMIT_WIDTH)?;
    if data.is_empty() {
        return Ok(vec![]);
    }
    let ref_time = db.newest_commit_at()?;
    let mut entropy_map: HashMap<i64, f64> = HashMap::new();
    for (file_id, committed_at, files_in_commit) in &data {
        let f = *files_in_commit as f64;
        if f <= 1.0 {
            continue;
        }
        let age_days = (ref_time - committed_at) as f64 / 86400.0;
        let decay = 2.0_f64.powf(-age_days / ENTROPY_HALF_LIFE_DAYS);
        let contribution = decay * (1.0 / f) * f.log2();
        *entropy_map.entry(*file_id).or_default() += contribution;
    }
    let paths = file_path_map(db)?;
    let findings = entropy_map
        .into_iter()
        .filter(|&(_, entropy)| entropy >= ENTROPY_THRESHOLD)
        .map(|(file_id, entropy)| {
            let path = paths.get(&file_id).map(|s| s.as_str()).unwrap_or("?");
            HealthFinding {
                file_id,
                symbol_id: None,
                biomarker_kind: BiomarkerKind::ChangeEntropy,
                severity: BiomarkerKind::ChangeEntropy.default_severity(),
                confidence: 1.0,
                provenance: "computed".into(),
                metric_value: entropy,
                threshold: ENTROPY_THRESHOLD,
                detail: format!(
                    "{path} has change entropy {entropy:.2} (threshold {ENTROPY_THRESHOLD})"
                ),
            }
        })
        .collect();
    Ok(findings)
}

pub fn compute_ownership_risk(db: &Db, workspace_root: &Path) -> Result<Vec<HealthFinding>> {
    let raw = db.file_author_commits()?;
    let owners_config = load_owners_config(workspace_root);
    let mut by_file: HashMap<i64, HashMap<String, i64>> = HashMap::new();
    for (file_id, author, count) in raw {
        let canonical = owners_config
            .aliases
            .get(&author)
            .cloned()
            .unwrap_or(author);
        *by_file
            .entry(file_id)
            .or_default()
            .entry(canonical)
            .or_default() += count;
    }
    let paths = file_path_map(db)?;
    let mut findings = Vec::new();
    for (file_id, author_counts) in &by_file {
        let total: i64 = author_counts.values().sum();
        if total == 0 {
            continue;
        }
        let max_share = author_counts
            .values()
            .map(|&c| c as f64 / total as f64)
            .fold(0.0_f64, f64::max);
        let minor_count = author_counts
            .values()
            .filter(|&&c| (c as f64 / total as f64) < OWNERSHIP_MINOR_THRESHOLD)
            .count();
        let top_trigger = max_share < OWNERSHIP_TOP_THRESHOLD;
        let minor_trigger = minor_count >= OWNERSHIP_MINOR_COUNT;
        if !top_trigger && !minor_trigger {
            continue;
        }
        let path = paths.get(file_id).map(|s| s.as_str()).unwrap_or("?");
        let detail = if top_trigger && minor_trigger {
            format!(
                "{path}: top owner {:.0}% (< 40%) and {minor_count} minor contributors",
                max_share * 100.0
            )
        } else if top_trigger {
            format!("{path}: top owner {:.0}% (< 40%)", max_share * 100.0)
        } else {
            format!("{path}: {minor_count} minor contributors (>= 3 with < 5% each)")
        };
        let metric = if top_trigger {
            max_share
        } else {
            minor_count as f64
        };
        let threshold = if top_trigger {
            OWNERSHIP_TOP_THRESHOLD
        } else {
            OWNERSHIP_MINOR_COUNT as f64
        };
        findings.push(HealthFinding {
            file_id: *file_id,
            symbol_id: None,
            biomarker_kind: BiomarkerKind::OwnershipRisk,
            severity: BiomarkerKind::OwnershipRisk.default_severity(),
            confidence: 1.0,
            provenance: "computed".into(),
            metric_value: metric,
            threshold,
            detail,
        });
    }
    Ok(findings)
}

pub fn compute_hidden_coupling(db: &Db) -> Result<Vec<HealthFinding>> {
    let cochange = db.cochange_pairs_above_threshold(HIDDEN_COUPLING_MIN)?;
    let static_edges: HashSet<(i64, i64)> = db.static_file_edges()?.into_iter().collect();
    let paths = file_path_map(db)?;
    let mut findings = Vec::new();
    for (fa, fb, jaccard, _shared) in cochange {
        let key = (fa.min(fb), fa.max(fb));
        if static_edges.contains(&key) {
            continue;
        }
        let severity = if jaccard >= HIDDEN_COUPLING_HIGH {
            HealthSeverity::Advisory
        } else {
            HealthSeverity::Informational
        };
        let path_a = paths.get(&fa).map(|s| s.as_str()).unwrap_or("?");
        let path_b = paths.get(&fb).map(|s| s.as_str()).unwrap_or("?");
        let pct = (jaccard * 100.0) as u32;
        for (file_id, other_path) in [(fa, path_b), (fb, path_a)] {
            findings.push(HealthFinding {
                file_id,
                symbol_id: None,
                biomarker_kind: BiomarkerKind::HiddenCoupling,
                severity,
                confidence: 1.0,
                provenance: "computed".into(),
                metric_value: jaccard,
                threshold: HIDDEN_COUPLING_MIN,
                detail: format!(
                    "hidden coupling with {other_path} at {pct}% co-change (no static edge)"
                ),
            });
        }
    }
    Ok(findings)
}
