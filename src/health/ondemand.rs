use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::db::Db;
use crate::error::Result;
use crate::git::{self, BlameLine};
use crate::health::evidence::{InputFailure, MissingReason, ProducerOutcome};
use crate::health::findings::{BiomarkerKind, HealthFinding};
use crate::health::scoring::ProducerResult;

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

/// Review-time evidence: fresh findings plus an explicit outcome for every
/// on-demand producer that applies to each indexed changed path. A path whose
/// blame or shape analysis failed carries `Missing`, never an empty `Complete` —
/// missing on-demand evidence is explicit (health-evidence-contract.md §
/// Comparison and scoring).
#[derive(Debug, Default)]
pub struct OnDemandEvidence<'a> {
    pub findings: Vec<HealthFinding>,
    /// Keyed by changed path (borrowed from the caller's changed-path list).
    pub outcomes: HashMap<&'a str, Vec<ProducerResult>>,
}

impl<'a> OnDemandEvidence<'a> {
    fn record(&mut self, path: &'a str, kind: BiomarkerKind, outcome: ProducerOutcome) {
        self.outcomes.entry(path).or_default().push((kind, outcome));
    }

    /// Outcomes recorded for `path` (empty when no on-demand producer applied).
    pub fn outcomes_for(&self, path: &str) -> &[ProducerResult] {
        self.outcomes.get(path).map_or(&[], Vec::as_slice)
    }

    /// Fold the shape diff in: subtle-structural changes become `HrrShapeChange`
    /// findings, analyzed paths `Complete`, failed paths `Missing`.
    pub fn add_shape_diff(
        &mut self,
        diff: &crate::similarity::diff::ShapeDiff<'a>,
        hrr_threshold: f64,
    ) {
        let findings = compute_shape_change_findings(&diff.changes, hrr_threshold);
        for &path in &diff.analyzed {
            let count = diff
                .changes
                .iter()
                .filter(|c| {
                    c.file_path == path
                        && c.file_id.is_some()
                        && c.quadrant == crate::similarity::diff::DiffQuadrant::SubtleStructural
                })
                .count();
            self.record(
                path,
                BiomarkerKind::HrrShapeChange,
                ProducerOutcome::Complete {
                    finding_count: count,
                },
            );
        }
        for &path in &diff.failed {
            self.record(
                path,
                BiomarkerKind::HrrShapeChange,
                ProducerOutcome::Missing(MissingReason::Failed(InputFailure::ProbeFailed)),
            );
        }
        self.findings.extend(findings);
    }
}

/// Blame-derived on-demand findings (function_hotspot, code_age_volatility) for
/// the changed paths. Storage errors propagate; a per-path blame failure is
/// recorded as `Missing` for both blame producers rather than skipped.
pub fn compute_ondemand_findings<'a>(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &'a [String],
) -> Result<OnDemandEvidence<'a>> {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut blame_cache = BlameCache::new();
    let mut all_stats: Vec<FunctionBlameStats> = Vec::new();
    let mut evidence = OnDemandEvidence::default();
    // Indexed paths whose blame succeeded, keyed by file id for the counts below.
    let mut blamed: Vec<(&str, i64)> = Vec::new();

    for path in changed_paths {
        // Unindexed (deleted/ignored) paths have no file to score.
        let Some(file_row) = db.file_by_path(path)? else {
            continue;
        };
        let blame_lines = match blame_cache.get_or_compute(workspace_root, path) {
            Ok(lines) => lines,
            Err(e) => {
                tracing::debug!(path, "on-demand blame failed: {e}");
                for kind in [
                    BiomarkerKind::FunctionHotspot,
                    BiomarkerKind::CodeAgeVolatility,
                ] {
                    evidence.record(
                        path,
                        kind,
                        ProducerOutcome::Missing(MissingReason::Failed(InputFailure::ProbeFailed)),
                    );
                }
                continue;
            }
        };
        blamed.push((path, file_row.id));
        if blame_lines.is_empty() {
            continue;
        }
        let symbols = db.find_symbols_by_file(file_row.id)?;

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
                qualified_name: sym.qualified_name.to_string(),
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

    for (path, file_id) in blamed {
        for kind in [
            BiomarkerKind::FunctionHotspot,
            BiomarkerKind::CodeAgeVolatility,
        ] {
            let count = findings
                .iter()
                .filter(|f| f.file_id == file_id && f.biomarker_kind == kind)
                .count();
            evidence.record(
                path,
                kind,
                ProducerOutcome::Complete {
                    finding_count: count,
                },
            );
        }
    }
    evidence.findings = findings;
    Ok(evidence)
}

fn is_function_kind(kind: &str) -> bool {
    matches!(
        kind,
        "function" | "method" | "function_item" | "function_declaration" | "method_declaration"
    )
}

/// Convert subtle-structural shape changes into HealthFindings so they feed the
/// health delta and scoring, not just the review's display list. Only the
/// SubtleStructural quadrant is debt (text barely changed but the structural
/// shape moved a lot — an easy-to-miss rewrite); other quadrants stay display-only.
pub fn compute_shape_change_findings(
    shape_changes: &[crate::similarity::diff::ShapeChange],
    hrr_threshold: f64,
) -> Vec<HealthFinding> {
    use crate::similarity::diff::DiffQuadrant;
    shape_changes
        .iter()
        .filter(|c| c.quadrant == DiffQuadrant::SubtleStructural)
        .filter_map(|c| {
            let file_id = c.file_id?;
            Some(HealthFinding {
                file_id,
                symbol_id: c.symbol_id,
                biomarker_kind: BiomarkerKind::HrrShapeChange,
                severity: BiomarkerKind::HrrShapeChange.default_severity(),
                confidence: 1.0,
                provenance: "on-demand:hrr".into(),
                metric_value: c.hrr_delta,
                threshold: hrr_threshold,
                detail: format!(
                    "{}: text changed {:.0}% but structural shape changed {:.0}%",
                    c.symbol_name,
                    c.text_delta * 100.0,
                    c.hrr_delta * 100.0
                ),
            })
        })
        .collect()
}
