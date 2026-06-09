use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::db::{Db, HealthFindingRow};
use crate::error::Result;
use crate::git::{self, BlameLine};
use crate::health::findings::{BiomarkerKind, HealthFinding};
use crate::health::scoring;

const HOTSPOT_CCN_THRESHOLD: i64 = 10;
const HOTSPOT_NESTING_THRESHOLD: i64 = 3;
const HOTSPOT_MIN_P80: usize = 5;

const AGE_DORMANT_DAYS: i64 = 365;
const AGE_RECENT_DAYS: i64 = 30;
const AGE_RECENT_COMMITS_THRESHOLD: usize = 2;

pub struct BlameCache {
    cache: HashMap<String, Vec<BlameLine>>,
}

impl Default for BlameCache {
    fn default() -> Self {
        Self::new()
    }
}

impl BlameCache {
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    pub fn get_or_compute(&mut self, workspace_root: &Path, path: &str) -> Result<&[BlameLine]> {
        if !self.cache.contains_key(path) {
            self.cache.insert(
                path.to_string(),
                git::git_blame_porcelain(workspace_root, path)?,
            );
        }
        Ok(self.cache.get(path).unwrap())
    }
}

struct FunctionBlameStats {
    symbol_id: i64,
    file_id: i64,
    qualified_name: String,
    distinct_commits: usize,
    cyclomatic: Option<i64>,
    max_nesting: Option<i64>,
    median_age_days: f64,
    recent_commits: usize,
}

pub fn compute_ondemand_findings(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
) -> Vec<HealthFinding> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut blame_cache = BlameCache::new();
    let mut all_stats: Vec<FunctionBlameStats> = Vec::new();

    for path in changed_paths {
        let blame_lines = match blame_cache.get_or_compute(workspace_root, path) {
            Ok(lines) => lines,
            Err(_) => continue,
        };
        if blame_lines.is_empty() {
            continue;
        }
        let file_row = match db.file_by_path(path) {
            Ok(Some(f)) => f,
            _ => continue,
        };
        let symbols = match db.find_symbols_by_file(file_row.id) {
            Ok(s) => s,
            _ => continue,
        };

        for sym in symbols.iter().filter(|s| is_function_kind(&s.kind)) {
            let fn_lines: Vec<&BlameLine> = blame_lines
                .iter()
                .filter(|bl| {
                    bl.line_no >= sym.start_line as usize && bl.line_no <= sym.end_line as usize
                })
                .collect();

            if fn_lines.is_empty() {
                continue;
            }

            let commits: HashSet<&str> = fn_lines.iter().map(|bl| bl.commit.as_str()).collect();

            let mut ages_days: Vec<f64> = fn_lines
                .iter()
                .map(|bl| (now_secs - bl.author_time) as f64 / 86400.0)
                .collect();
            ages_days.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let median_age = ages_days[ages_days.len() / 2];

            let cutoff = now_secs - (AGE_RECENT_DAYS * 86400);
            let recent: HashSet<&str> = fn_lines
                .iter()
                .filter(|bl| bl.author_time >= cutoff)
                .map(|bl| bl.commit.as_str())
                .collect();

            all_stats.push(FunctionBlameStats {
                symbol_id: sym.id,
                file_id: file_row.id,
                qualified_name: sym.qualified_name.clone(),
                distinct_commits: commits.len(),
                cyclomatic: sym.cyclomatic,
                max_nesting: sym.max_nesting,
                median_age_days: median_age,
                recent_commits: recent.len(),
            });
        }
    }

    let mut commit_counts: Vec<usize> = all_stats.iter().map(|s| s.distinct_commits).collect();
    commit_counts.sort();
    let p80 = if commit_counts.is_empty() {
        HOTSPOT_MIN_P80
    } else {
        let idx = (commit_counts.len() as f64 * 0.8) as usize;
        commit_counts[idx.min(commit_counts.len() - 1)].max(HOTSPOT_MIN_P80)
    };

    let mut findings = Vec::new();

    for stat in &all_stats {
        if stat.distinct_commits >= p80 {
            let ccn = stat.cyclomatic.unwrap_or(0);
            let nesting = stat.max_nesting.unwrap_or(0);
            if ccn >= HOTSPOT_CCN_THRESHOLD || nesting >= HOTSPOT_NESTING_THRESHOLD {
                findings.push(HealthFinding {
                    file_id: stat.file_id,
                    symbol_id: Some(stat.symbol_id),
                    biomarker_kind: BiomarkerKind::FunctionHotspot,
                    severity: BiomarkerKind::FunctionHotspot.default_severity(),
                    confidence: 1.0,
                    provenance: "on-demand:blame".into(),
                    metric_value: stat.distinct_commits as f64,
                    threshold: p80 as f64,
                    detail: format!(
                        "{}: {} distinct commits (p80={}), ccn={}, nesting={}",
                        stat.qualified_name, stat.distinct_commits, p80, ccn, nesting,
                    ),
                });
            }
        }

        if stat.median_age_days >= AGE_DORMANT_DAYS as f64
            && stat.recent_commits >= AGE_RECENT_COMMITS_THRESHOLD
        {
            findings.push(HealthFinding {
                file_id: stat.file_id,
                symbol_id: Some(stat.symbol_id),
                biomarker_kind: BiomarkerKind::CodeAgeVolatility,
                severity: BiomarkerKind::CodeAgeVolatility.default_severity(),
                confidence: 1.0,
                provenance: "on-demand:blame".into(),
                metric_value: stat.median_age_days,
                threshold: AGE_DORMANT_DAYS as f64,
                detail: format!(
                    "{}: median age {:.0}d with {} recent commits in last {}d",
                    stat.qualified_name, stat.median_age_days, stat.recent_commits, AGE_RECENT_DAYS,
                ),
            });
        }
    }

    findings
}

fn is_function_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function" | "method" | "function_item" | "function_declaration" | "method_declaration"
    )
}

// --- Health delta ---

pub struct HealthDeltaEntry {
    pub path: String,
    pub previous_score: f64,
    pub current_score: f64,
    pub delta: f64,
    pub driving_findings: Vec<HealthFindingRow>,
}

pub struct HealthDelta {
    pub degraded: Vec<HealthDeltaEntry>,
    pub improved: Vec<HealthDeltaEntry>,
}

pub fn compute_health_delta(
    db: &Db,
    changed_paths: &[String],
    ondemand_findings: &[HealthFinding],
) -> Result<HealthDelta> {
    let snapshots = db.latest_snapshots(1)?;
    let snapshot_scores: HashMap<String, f64> = if let Some(snap) = snapshots.first() {
        db.snapshot_file_scores(snap.id)?
            .into_iter()
            .map(|f| (f.file_path, f.score))
            .collect()
    } else {
        HashMap::new()
    };

    let mut degraded = Vec::new();
    let mut improved = Vec::new();

    for path in changed_paths {
        let file_row = match db.file_by_path(path) {
            Ok(Some(f)) => f,
            _ => continue,
        };

        let prev_score = snapshot_scores.get(path).copied().unwrap_or(10.0);

        let stored = db.get_health_findings(Some(file_row.id), None)?;
        let ondemand_rows: Vec<HealthFindingRow> = ondemand_findings
            .iter()
            .filter(|f| f.file_id == file_row.id)
            .enumerate()
            .map(|(i, f)| f.to_row(-(i as i64) - 1))
            .collect();

        let mut all_findings = stored;
        all_findings.extend(ondemand_rows.clone());

        let health = scoring::score_file(&all_findings);
        let delta = health.score - prev_score;

        if delta.abs() < 0.005 {
            continue;
        }

        let driving = if delta < 0.0 { ondemand_rows } else { vec![] };

        let entry = HealthDeltaEntry {
            path: path.clone(),
            previous_score: prev_score,
            current_score: health.score,
            delta,
            driving_findings: driving,
        };

        if delta < 0.0 {
            degraded.push(entry);
        } else {
            improved.push(entry);
        }
    }

    degraded.sort_by(|a, b| {
        a.delta
            .partial_cmp(&b.delta)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    improved.sort_by(|a, b| {
        b.delta
            .partial_cmp(&a.delta)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(HealthDelta { degraded, improved })
}
