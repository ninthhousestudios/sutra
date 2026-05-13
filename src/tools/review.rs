use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::dd::{DdEngine, DdFacts};
use crate::error::Result;
use crate::fca::{self, FcaEngine};
use crate::freshness::{self, FreshnessLevel};
use crate::git;
use crate::rules;
use crate::tools::scoring::{self, ChurnMap, Signal};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReviewArgs {
    pub workspace: String,
    /// "branch" (default), "staged", or "unstaged"
    #[serde(default)]
    pub diff: Option<String>,
}

const MAX_AFFECTED: usize = 20;
const MAX_READS: usize = 10;

const W_BLAST: f64 = 0.30;
const W_COMPLEXITY: f64 = 0.20;
const W_HOTSPOT: f64 = 0.15;
const W_CHURN: f64 = 0.15;
const W_CONVENTIONS: f64 = 0.20;

#[derive(Clone)]
pub struct ConstraintViolation {
    pub kind: String,
    pub from_path: String,
    pub to_path: String,
    pub detail: String,
}

#[derive(Clone)]
pub struct ConventionViolation {
    pub symbol: String,
    pub file: String,
    pub convention_id: String,
    pub antecedent: Vec<String>,
    pub consequent: Vec<String>,
    pub missing: Vec<String>,
    pub support: usize,
    pub confidence: f64,
}

#[derive(Default)]
pub struct ReviewFindings {
    pub constraint_violations: Vec<ConstraintViolation>,
    pub convention_violations: Vec<ConventionViolation>,
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    diff_mode: Option<&str>,
    dd_engine: Option<&DdEngine>,
) -> Result<serde_json::Value> {
    let mode = diff_mode.unwrap_or("branch");

    let changed_paths = match mode {
        "staged" => git::git_diff_staged(workspace_root)?,
        "unstaged" => git::git_diff_unstaged(workspace_root)?,
        _ => {
            let default_branch = git::detect_default_branch(workspace_root)?;
            let base = git::git_merge_base(workspace_root, &default_branch)?;
            git::git_diff_files(workspace_root, &base, "HEAD")?
        }
    };

    let churn = ChurnMap {
        counts: git::git_churn(workspace_root, scoring::CHURN_WINDOW_DAYS)?,
        window_days: scoring::CHURN_WINDOW_DAYS,
    };

    let (findings, findings_error) =
        match build_findings(db, workspace_root, &changed_paths, dd_engine) {
            Ok(f) => (f, None),
            Err(e) => (ReviewFindings::default(), Some(e.to_string())),
        };

    let mut result = compute(db, workspace_root, &changed_paths, &churn, &findings)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("diff_mode".into(), json!(mode));
        obj.insert("churn_window_days".into(), json!(scoring::CHURN_WINDOW_DAYS));
        if let Some(err) = findings_error {
            obj.insert("findings_degraded".into(), json!(true));
            obj.insert("findings_error".into(), json!(err));
        }
    }
    Ok(result)
}

pub fn build_findings(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    shared_dd: Option<&DdEngine>,
) -> Result<ReviewFindings> {
    let rules = rules::load_rules(workspace_root)?;
    let all_files = db.all_files()?;
    let path_map: HashMap<i64, String> = all_files.iter().map(|f| (f.id, f.path.clone())).collect();
    let id_map: HashMap<&str, i64> = all_files.iter().map(|f| (f.path.as_str(), f.id)).collect();

    // DD: forbidden deps + cycles involving changed files
    let mut constraint_violations = Vec::new();

    let ephemeral;
    let edges = db.import_edges()?;
    let dd: Option<&DdEngine> = if !edges.is_empty() {
        if let Some(engine) = shared_dd.filter(|e| !e.is_invalidated()) {
            if !engine.is_loaded() {
                engine.ingest(DdFacts {
                    import_edges: edges,
                })?;
            }
            Some(engine)
        } else {
            ephemeral = DdEngine::new(Duration::from_secs(60));
            ephemeral.ingest(DdFacts {
                import_edges: edges,
            })?;
            Some(&ephemeral)
        }
    } else {
        None
    };

    if let Some(engine) = dd {
        for v in engine.query_forbidden_deps(&rules.constraints.forbidden_deps, &path_map)? {
            let from_path = path_map.get(&v.from_id).cloned().unwrap_or_default();
            let to_path = path_map.get(&v.to_id).cloned().unwrap_or_default();
            if changed_paths.contains(&from_path) || changed_paths.contains(&to_path) {
                constraint_violations.push(ConstraintViolation {
                    kind: "forbidden_dep".into(),
                    from_path: from_path.clone(),
                    to_path: to_path.clone(),
                    detail: format!(
                        "forbidden: {} -> {} (rule: {} -> {})",
                        from_path, to_path, v.rule_from, v.rule_to
                    ),
                });
            }
        }

        let changed_ids: std::collections::HashSet<i64> = changed_paths
            .iter()
            .filter_map(|p| id_map.get(p.as_str()).copied())
            .collect();

        if !changed_ids.is_empty() {
            for cycle in engine.query_cycles()? {
                if cycle.file_ids.iter().any(|id| changed_ids.contains(id)) {
                    let cycle_paths: Vec<String> = cycle
                        .file_ids
                        .iter()
                        .filter_map(|id| path_map.get(id).cloned())
                        .collect();
                    constraint_violations.push(ConstraintViolation {
                        kind: "cycle".into(),
                        from_path: cycle_paths.first().cloned().unwrap_or_default(),
                        to_path: cycle_paths.last().cloned().unwrap_or_default(),
                        detail: format!("import cycle: {}", cycle_paths.join(" -> ")),
                    });
                }
            }
        }
    }

    // FCA: convention violations on changed symbols
    let mut convention_violations = Vec::new();

    let mut all_sym_attrs = Vec::new();
    for f in &all_files {
        let syms = db.find_symbols_by_file(f.id)?;
        for s in &syms {
            if let Some(attrs) = fca::extract_symbol_attrs(&s, &f.path) {
                all_sym_attrs.push(attrs);
            }
        }
    }

    if !all_sym_attrs.is_empty() {
        let mut fca_engine = FcaEngine::new();
        let conventions = fca_engine.rebuild(&all_sym_attrs);

        for c in &conventions {
            let _ = db.upsert_convention(
                &c.id,
                &c.antecedent.join(", "),
                &c.consequent.join(", "),
                c.support as i64,
                c.confidence,
            );
        }
        let current_ids: Vec<&str> = conventions.iter().map(|c| c.id.as_str()).collect();
        let _ = db.delete_stale_conventions(&current_ids);

        let changed_set: std::collections::HashSet<&str> =
            changed_paths.iter().map(|p| p.as_str()).collect();
        let changed_sym_attrs: Vec<_> = all_sym_attrs
            .iter()
            .filter(|a| changed_set.contains(a.file.as_str()))
            .cloned()
            .collect();

        for v in fca_engine.check(&changed_sym_attrs, &rules.conventions) {
            convention_violations.push(ConventionViolation {
                symbol: v.symbol,
                file: v.file,
                convention_id: v.convention_id,
                antecedent: v.antecedent,
                consequent: v.consequent,
                missing: v.missing,
                support: v.support,
                confidence: v.confidence,
            });
        }
    }

    Ok(ReviewFindings {
        constraint_violations,
        convention_violations,
    })
}

struct ChangeStats {
    changed_files: Vec<serde_json::Value>,
    changed_symbols: Vec<serde_json::Value>,
    symbol_ids: Vec<i64>,
    total_blast: i64,
    max_cognitive: i64,
    total_churn: u32,
    hotspot_files: u32,
}

fn gather_change_stats(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    churn: &ChurnMap,
) -> Result<ChangeStats> {
    let mut stats = ChangeStats {
        changed_files: Vec::new(),
        changed_symbols: Vec::new(),
        symbol_ids: Vec::new(),
        total_blast: 0,
        max_cognitive: 0,
        total_churn: 0,
        hotspot_files: 0,
    };

    for path in changed_paths {
        let file_churn = churn.counts.get(path).copied().unwrap_or(0);
        stats.total_churn += file_churn;

        if let Some(file) = db.file_by_path(path)? {
            let fl: FreshnessLevel =
                freshness::check_file(workspace_root, path, &file.last_parsed).into();
            stats.total_blast += file.blast_radius;
            if file_churn > 5 && file.blast_radius > 10 {
                stats.hotspot_files += 1;
            }
            let syms = db.find_symbols_by_file(file.id)?;
            for s in &syms {
                stats.symbol_ids.push(s.id);
                let cog = s.cognitive.unwrap_or(0);
                if cog > stats.max_cognitive {
                    stats.max_cognitive = cog;
                }
                stats.changed_symbols.push(json!({
                    "symbol": s.qualified_name, "file": path, "cognitive": cog,
                }));
            }
            stats.changed_files.push(json!({
                "path": path, "blast_radius": file.blast_radius, "symbol_count": syms.len(),
                "_freshness": fl,
            }));
        } else {
            stats.changed_files.push(json!({
                "path": path, "blast_radius": 0, "symbol_count": 0,
                "_freshness": FreshnessLevel::StaleIndex,
            }));
        }
    }
    Ok(stats)
}

fn gather_affected(
    db: &Db,
    symbol_ids: &[i64],
    changed_paths: &[String],
) -> Result<(Vec<(String, i64)>, Vec<(String, String, i64, i64)>)> {
    let affected_file_ids = if symbol_ids.is_empty() {
        Vec::new()
    } else {
        db.find_files_referencing_symbols(symbol_ids)?
    };

    let changed_set: std::collections::HashSet<&str> =
        changed_paths.iter().map(|p| p.as_str()).collect();

    let mut files = Vec::new();
    let mut symbols = Vec::new();

    for fid in &affected_file_ids {
        if let Some(file) = db.file_by_id(*fid)? {
            if changed_set.contains(file.path.as_str()) {
                continue;
            }
            for s in db.find_symbols_by_file(file.id)? {
                symbols.push((
                    s.qualified_name.clone(),
                    file.path.clone(),
                    file.blast_radius,
                    s.cognitive.unwrap_or(0),
                ));
            }
            files.push((file.path.clone(), file.blast_radius));
        }
    }

    files.sort_by(|a, b| b.1.cmp(&a.1));
    files.dedup_by(|a, b| a.0 == b.0);
    symbols.sort_by(|a, b| b.2.cmp(&a.2));
    symbols.dedup_by(|a, b| a.0 == b.0);

    Ok((files, symbols))
}

fn file_freshness(db: &Db, workspace_root: &Path, path: &str) -> FreshnessLevel {
    db.file_by_path(path)
        .ok()
        .flatten()
        .map(|f| freshness::check_file(workspace_root, path, &f.last_parsed).into())
        .unwrap_or(FreshnessLevel::StaleIndex)
}

fn build_recommended_reads(
    db: &Db,
    workspace_root: &Path,
    findings: &ReviewFindings,
    affected_files: &[(String, i64)],
    changed_files: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut violation_sites: Vec<(String, i64, f64)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for v in &findings.convention_violations {
        if !seen.insert(v.file.clone()) {
            continue;
        }
        let blast = affected_files
            .iter()
            .find(|(p, _)| p == &v.file)
            .map(|(_, b)| *b)
            .or_else(|| {
                changed_files
                    .iter()
                    .find(|cf| cf["path"].as_str() == Some(v.file.as_str()))
                    .and_then(|cf| cf["blast_radius"].as_i64())
            })
            .unwrap_or(0);
        violation_sites.push((v.file.clone(), blast, v.confidence));
    }
    violation_sites.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut reads: Vec<(String, i64, bool)> = Vec::new();
    for (path, blast, _) in &violation_sites {
        reads.push((path.clone(), *blast, true));
    }
    for (path, blast) in affected_files {
        if !seen.contains(path) {
            reads.push((path.clone(), *blast, false));
        }
    }
    reads.truncate(MAX_READS);

    reads.iter().map(|(path, blast, is_violation)| {
        let fl = file_freshness(db, workspace_root, path);
        json!({ "path": path, "blast_radius": blast, "violation_site": is_violation, "_freshness": fl })
    }).collect()
}

pub fn compute(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    churn: &ChurnMap,
    findings: &ReviewFindings,
) -> Result<serde_json::Value> {
    if changed_paths.is_empty() {
        return Ok(json!({
            "changed_files": [],
            "changed_symbols": [],
            "affected_files": [],
            "affected_symbols": [],
            "affected_total": { "files": 0, "symbols": 0 },
            "risk_score": 0.0,
            "risk_breakdown": {
                "blast_radius": 0.0, "complexity_delta": 0.0,
                "hotspot_overlap": 0.0, "churn": 0.0, "convention_violations": 0.0,
            },
            "recommended_reads": [],
            "constraint_violations": [],
            "convention_violations": [],
        }));
    }

    let stats = gather_change_stats(db, workspace_root, changed_paths, churn)?;
    let (affected_files, affected_symbols) = gather_affected(db, &stats.symbol_ids, changed_paths)?;

    let total_affected_files = affected_files.len();
    let total_affected_symbols = affected_symbols.len();

    let affected_files_out: Vec<_> = affected_files
        .iter()
        .take(MAX_AFFECTED)
        .map(|(path, blast)| {
            let fl = file_freshness(db, workspace_root, path);
            json!({ "path": path, "blast_radius": blast, "_freshness": fl })
        })
        .collect();
    let affected_symbols_out: Vec<_> = affected_symbols.iter().take(MAX_AFFECTED)
        .map(|(sym, file, blast, cog)| {
            let fl = file_freshness(db, workspace_root, file);
            json!({ "symbol": sym, "file": file, "blast_radius": blast, "cognitive": cog, "_freshness": fl })
        }).collect();

    let constraint_violations_out: Vec<_> = findings
        .constraint_violations
        .iter()
        .map(
            |v| json!({ "kind": v.kind, "from": v.from_path, "to": v.to_path, "detail": v.detail }),
        )
        .collect();
    let convention_violations_out: Vec<_> = findings
        .convention_violations
        .iter()
        .map(|v| {
            json!({
                "symbol": v.symbol, "file": v.file, "convention_id": v.convention_id,
                "antecedent": v.antecedent, "consequent": v.consequent, "missing": v.missing,
                "support": v.support, "confidence": v.confidence,
            })
        })
        .collect();

    let file_count = changed_paths.len();
    let blast_score = scoring::normalize(stats.total_blast as f64, scoring::BLAST_NORM);
    let complexity_score = scoring::normalize(stats.max_cognitive as f64, scoring::COMPLEXITY_NORM);
    let hotspot_score = scoring::normalize(
        stats.hotspot_files as f64,
        (file_count as f64).max(1.0),
    );
    let churn_score = scoring::normalize(stats.total_churn as f64, scoring::CHURN_NORM);
    let convention_score = scoring::normalize(findings.convention_violations.len() as f64, 5.0);

    let risk_score = scoring::weighted_score(&[
        Signal { weight: W_BLAST, score: blast_score },
        Signal { weight: W_COMPLEXITY, score: complexity_score },
        Signal { weight: W_HOTSPOT, score: hotspot_score },
        Signal { weight: W_CHURN, score: churn_score },
        Signal { weight: W_CONVENTIONS, score: convention_score },
    ]);

    let recommended_reads =
        build_recommended_reads(db, workspace_root, findings, &affected_files, &stats.changed_files);

    Ok(json!({
        "changed_files": stats.changed_files,
        "changed_symbols": stats.changed_symbols,
        "affected_files": affected_files_out,
        "affected_symbols": affected_symbols_out,
        "affected_total": {
            "files": total_affected_files,
            "files_truncated": total_affected_files > MAX_AFFECTED,
            "symbols": total_affected_symbols,
            "symbols_truncated": total_affected_symbols > MAX_AFFECTED,
        },
        "risk_score": scoring::round3(risk_score),
        "risk_breakdown": {
            "blast_radius": scoring::round3(blast_score),
            "complexity_delta": scoring::round3(complexity_score),
            "hotspot_overlap": scoring::round3(hotspot_score),
            "churn": scoring::round3(churn_score),
            "convention_violations": scoring::round3(convention_score),
        },
        "constraint_violations": constraint_violations_out,
        "convention_violations": convention_violations_out,
        "recommended_reads": recommended_reads,
    }))
}
