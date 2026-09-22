use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
// blast_radius_churn: a file that many things transitively depend on AND that
// changes often is a structural risk (every edit ripples widely). PROVISIONAL
// absolute thresholds — not repowise-calibrated (corpus unavailable in this
// repo). Weight 1.00 (moderate), Advisory (structural category).
const BLAST_RADIUS_THRESHOLD: i64 = 10;
const BLAST_CHURN_THRESHOLD: i64 = 5;

#[derive(Debug, Default, Deserialize)]
pub struct OwnersConfig {
    #[serde(default)]
    pub aliases: HashMap<String, String>,
}

/// The widest commit (in indexed files touched) a git producer consumes; wider
/// commits are discarded as carrying no co-edit signal. A file whose only
/// in-window commits exceed its producer's limit has no usable history for that
/// producer, so the run stages it Missing(NoHistory) (sutra/423). `None` = the
/// producer applies no width filter.
pub(crate) fn max_observed_commit_width(kind: BiomarkerKind) -> Option<i64> {
    match kind {
        BiomarkerKind::ChangeEntropy => Some(MAX_COMMIT_WIDTH),
        BiomarkerKind::HiddenCoupling => Some(crate::db::MAX_COCHANGE_COMMIT_FANOUT),
        _ => None,
    }
}

fn file_path_map(db: &Db) -> Result<HashMap<i64, Arc<str>>> {
    Ok(db
        .all_files()?
        .into_iter()
        .map(|f| (f.id, f.path))
        .collect())
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
            let path = paths.get(&file_id).map(|s| &**s).unwrap_or("?");
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
            let path = paths.get(&file_id).map(|s| &**s).unwrap_or("?");
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

/// Ownership-risk findings from the observed author distribution, merged through
/// the caller-supplied owners aliases. The config is *probed* by the caller
/// ([`crate::health::probe::probe_owners`]) so a malformed/unreadable owners file
/// is a recorded failure the caller declines to score from, never silently
/// treated as an empty default (health-evidence contract, sutra/415).
pub fn compute_ownership_risk(db: &Db, owners_config: &OwnersConfig) -> Result<Vec<HealthFinding>> {
    let raw = db.file_author_commits()?;
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
        let path = paths.get(file_id).map(|s| &**s).unwrap_or("?");
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
        let path_a = paths.get(&fa).map(|s| &**s).unwrap_or("?");
        let path_b = paths.get(&fb).map(|s| &**s).unwrap_or("?");
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

pub fn compute_blast_radius_churn(db: &Db) -> Result<Vec<HealthFinding>> {
    // Churn = distinct commits touching the file, summed across authors.
    let mut churn: HashMap<i64, i64> = HashMap::new();
    for (file_id, _author, count) in db.file_author_commits()? {
        *churn.entry(file_id).or_default() += count;
    }
    if churn.is_empty() {
        return Ok(vec![]);
    }
    let findings = db
        .all_files()?
        .into_iter()
        .filter_map(|f| {
            let commits = churn.get(&f.id).copied().unwrap_or(0);
            if f.blast_radius < BLAST_RADIUS_THRESHOLD || commits < BLAST_CHURN_THRESHOLD {
                return None;
            }
            Some(HealthFinding {
                file_id: f.id,
                symbol_id: None,
                biomarker_kind: BiomarkerKind::BlastRadiusChurn,
                severity: BiomarkerKind::BlastRadiusChurn.default_severity(),
                confidence: 1.0,
                provenance: "computed".into(),
                metric_value: f.blast_radius as f64,
                threshold: BLAST_RADIUS_THRESHOLD as f64,
                detail: format!(
                    "{} transitive dependents and {commits} commits (high-churn, widely depended on)",
                    f.blast_radius
                ),
            })
        })
        .collect();
    Ok(findings)
}
