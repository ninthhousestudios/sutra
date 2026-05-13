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
use crate::git;
use crate::rules;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReviewArgs {
    pub workspace: String,
    /// "branch" (default), "staged", or "unstaged"
    #[serde(default)]
    pub diff: Option<String>,
}

const CHURN_WINDOW_DAYS: u32 = 90;
const MAX_AFFECTED: usize = 20;
const MAX_READS: usize = 10;

const W_BLAST: f64 = 0.30;
const W_COMPLEXITY: f64 = 0.20;
const W_HOTSPOT: f64 = 0.15;
const W_CHURN: f64 = 0.15;
const W_CONVENTIONS: f64 = 0.20;

#[derive(Default)]
pub struct ChurnMap {
    pub counts: HashMap<String, u32>,
}

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
) -> Result<serde_json::Value> {
    let mode = diff_mode.unwrap_or("branch");

    let changed_paths = match mode {
        "staged" => git::git_diff_staged(workspace_root)?,
        "unstaged" => git::git_diff_unstaged(workspace_root)?,
        _ => {
            let base = git::git_merge_base(workspace_root, "main")?;
            git::git_diff_files(workspace_root, &base, "HEAD")?
        }
    };

    let churn = ChurnMap {
        counts: git::git_churn(workspace_root, CHURN_WINDOW_DAYS)?,
    };

    let findings = build_findings(db, workspace_root, &changed_paths).unwrap_or_default();

    let mut result = compute(db, &changed_paths, &churn, &findings)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("diff_mode".into(), json!(mode));
        obj.insert("churn_window_days".into(), json!(CHURN_WINDOW_DAYS));
    }
    Ok(result)
}

fn build_findings(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
) -> Result<ReviewFindings> {
    let rules = rules::load_rules(workspace_root)?;
    let all_files = db.all_files()?;
    let path_map: HashMap<i64, String> =
        all_files.iter().map(|f| (f.id, f.path.clone())).collect();
    let id_map: HashMap<&str, i64> =
        all_files.iter().map(|f| (f.path.as_str(), f.id)).collect();

    // DD: forbidden deps + cycles involving changed files
    let mut constraint_violations = Vec::new();

    let edges = db.import_edges()?;
    if !edges.is_empty() {
        let engine = DdEngine::new(Duration::from_secs(60));
        engine.ingest(DdFacts { import_edges: edges })?;

        for v in engine.query_forbidden_deps(&rules.constraints.forbidden_deps, &path_map)? {
            let from_path = path_map.get(&v.from_id).cloned().unwrap_or_default();
            let to_path = path_map.get(&v.to_id).cloned().unwrap_or_default();
            if changed_paths.contains(&from_path) || changed_paths.contains(&to_path) {
                constraint_violations.push(ConstraintViolation {
                    kind: "forbidden_dep".into(),
                    from_path: from_path.clone(),
                    to_path: to_path.clone(),
                    detail: format!("forbidden: {} -> {} (rule: {} -> {})",
                        from_path, to_path, v.rule_from, v.rule_to),
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
                    let cycle_paths: Vec<String> = cycle.file_ids.iter()
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
        fca_engine.rebuild(&all_sym_attrs);

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
            });
        }
    }

    Ok(ReviewFindings {
        constraint_violations,
        convention_violations,
    })
}

pub fn compute(
    db: &Db,
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
                "blast_radius": 0.0,
                "complexity_delta": 0.0,
                "hotspot_overlap": 0.0,
                "churn": 0.0,
                "convention_violations": 0.0,
            },
            "recommended_reads": [],
            "constraint_violations": [],
            "convention_violations": [],
        }));
    }

    let mut changed_files = Vec::new();
    let mut changed_symbols = Vec::new();
    let mut all_symbol_ids = Vec::new();
    let mut total_blast: i64 = 0;
    let mut max_cognitive: i64 = 0;
    let mut total_churn: u32 = 0;
    let mut hotspot_files: u32 = 0;

    for path in changed_paths {
        let file_churn = churn.counts.get(path).copied().unwrap_or(0);
        total_churn += file_churn;

        if let Some(file) = db.file_by_path(path)? {
            total_blast += file.blast_radius;

            if file_churn > 5 && file.blast_radius > 10 {
                hotspot_files += 1;
            }

            let syms = db.find_symbols_by_file(file.id)?;
            for s in &syms {
                all_symbol_ids.push(s.id);
                let cog = s.cognitive.unwrap_or(0);
                if cog > max_cognitive {
                    max_cognitive = cog;
                }
                changed_symbols.push(json!({
                    "symbol": s.qualified_name,
                    "file": path,
                    "cognitive": cog,
                }));
            }
            changed_files.push(json!({
                "path": path,
                "blast_radius": file.blast_radius,
                "symbol_count": syms.len(),
            }));
        } else {
            changed_files.push(json!({
                "path": path,
                "blast_radius": 0,
                "symbol_count": 0,
            }));
        }
    }

    // Find affected files and symbols (transitive impact)
    let affected_file_ids = if all_symbol_ids.is_empty() {
        Vec::new()
    } else {
        db.find_files_referencing_symbols(&all_symbol_ids)?
    };

    let changed_set: std::collections::HashSet<&str> =
        changed_paths.iter().map(|p| p.as_str()).collect();

    let mut affected_files_full = Vec::new();
    let mut affected_symbols_full = Vec::new();

    for fid in &affected_file_ids {
        if let Some(file) = db.file_by_id(*fid)? {
            if changed_set.contains(file.path.as_str()) {
                continue;
            }
            let syms = db.find_symbols_by_file(file.id)?;
            for s in &syms {
                affected_symbols_full.push((
                    s.qualified_name.clone(),
                    file.path.clone(),
                    file.blast_radius,
                    s.cognitive.unwrap_or(0),
                ));
            }
            affected_files_full.push((file.path.clone(), file.blast_radius));
        }
    }

    // Deduplicate affected files
    affected_files_full.sort_by(|a, b| b.1.cmp(&a.1));
    affected_files_full.dedup_by(|a, b| a.0 == b.0);

    let total_affected_files = affected_files_full.len();
    let truncated_files = total_affected_files > MAX_AFFECTED;

    let affected_files_out: Vec<_> = affected_files_full
        .iter()
        .take(MAX_AFFECTED)
        .map(|(path, blast)| json!({ "path": path, "blast_radius": blast }))
        .collect();

    // Sort affected symbols by blast_radius desc for truncation
    let mut affected_syms_sorted = affected_symbols_full;
    affected_syms_sorted.sort_by(|a, b| b.2.cmp(&a.2));
    affected_syms_sorted.dedup_by(|a, b| a.0 == b.0);
    let total_affected_symbols = affected_syms_sorted.len();
    let truncated_symbols = total_affected_symbols > MAX_AFFECTED;

    let affected_symbols_out: Vec<_> = affected_syms_sorted
        .iter()
        .take(MAX_AFFECTED)
        .map(|(sym, file, blast, cog)| {
            json!({ "symbol": sym, "file": file, "blast_radius": blast, "cognitive": cog })
        })
        .collect();

    // Serialize findings
    let constraint_violations_out: Vec<_> = findings.constraint_violations.iter().map(|v| {
        json!({
            "kind": v.kind,
            "from": v.from_path,
            "to": v.to_path,
            "detail": v.detail,
        })
    }).collect();

    let convention_violations_out: Vec<_> = findings.convention_violations.iter().map(|v| {
        json!({
            "symbol": v.symbol,
            "file": v.file,
            "convention_id": v.convention_id,
            "antecedent": v.antecedent,
            "consequent": v.consequent,
            "missing": v.missing,
        })
    }).collect();

    // Risk score
    let file_count = changed_paths.len();
    let blast_score = (total_blast as f64 / 50.0).min(1.0);
    let complexity_score = (max_cognitive as f64 / 30.0).min(1.0);
    let hotspot_score = (hotspot_files as f64 / (file_count as f64).max(1.0)).min(1.0);
    let churn_score = (total_churn as f64 / 20.0).min(1.0);
    let convention_score = (findings.convention_violations.len() as f64 / 5.0).min(1.0);

    let risk_score = (W_BLAST * blast_score
        + W_COMPLEXITY * complexity_score
        + W_HOTSPOT * hotspot_score
        + W_CHURN * churn_score
        + W_CONVENTIONS * convention_score)
        .min(1.0);

    // Recommended reads: convention violation sites first, then affected files by blast_radius
    let violation_files: std::collections::HashSet<&str> = findings
        .convention_violations
        .iter()
        .map(|v| v.file.as_str())
        .collect();

    let mut reads: Vec<(String, i64, bool)> = Vec::new();

    // Violation sites first (with their blast radius)
    for vf in &violation_files {
        let blast = affected_files_full
            .iter()
            .find(|(p, _)| p == vf)
            .map(|(_, b)| *b)
            .or_else(|| {
                changed_files.iter()
                    .find(|cf| cf["path"].as_str() == Some(vf))
                    .and_then(|cf| cf["blast_radius"].as_i64())
            })
            .unwrap_or(0);
        reads.push((vf.to_string(), blast, true));
    }

    // Then affected files not already included
    for (path, blast) in &affected_files_full {
        if !violation_files.contains(path.as_str()) {
            reads.push((path.clone(), *blast, false));
        }
    }
    reads.truncate(MAX_READS);

    let recommended_reads: Vec<_> = reads
        .iter()
        .map(|(path, blast, is_violation_site)| {
            json!({
                "path": path,
                "blast_radius": blast,
                "violation_site": is_violation_site,
            })
        })
        .collect();

    Ok(json!({
        "changed_files": changed_files,
        "changed_symbols": changed_symbols,
        "affected_files": affected_files_out,
        "affected_symbols": affected_symbols_out,
        "affected_total": {
            "files": total_affected_files,
            "files_truncated": truncated_files,
            "symbols": total_affected_symbols,
            "symbols_truncated": truncated_symbols,
        },
        "risk_score": (risk_score * 1000.0).round() / 1000.0,
        "risk_breakdown": {
            "blast_radius": (blast_score * 1000.0).round() / 1000.0,
            "complexity_delta": (complexity_score * 1000.0).round() / 1000.0,
            "hotspot_overlap": (hotspot_score * 1000.0).round() / 1000.0,
            "churn": (churn_score * 1000.0).round() / 1000.0,
            "convention_violations": (convention_score * 1000.0).round() / 1000.0,
        },
        "constraint_violations": constraint_violations_out,
        "convention_violations": convention_violations_out,
        "recommended_reads": recommended_reads,
    }))
}
