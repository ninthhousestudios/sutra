use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::git;

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

const W_BLAST: f64 = 0.35;
const W_COMPLEXITY: f64 = 0.25;
const W_HOTSPOT: f64 = 0.20;
const W_CHURN: f64 = 0.20;

#[derive(Default)]
pub struct ChurnMap {
    pub counts: HashMap<String, u32>,
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

    let mut result = compute(db, &changed_paths, &churn)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("diff_mode".into(), json!(mode));
        obj.insert("churn_window_days".into(), json!(CHURN_WINDOW_DAYS));
    }
    Ok(result)
}

pub fn compute(
    db: &Db,
    changed_paths: &[String],
    churn: &ChurnMap,
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
            },
            "recommended_reads": [],
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

    // Risk score
    let file_count = changed_paths.len();
    let blast_score = (total_blast as f64 / 50.0).min(1.0);
    let complexity_score = (max_cognitive as f64 / 30.0).min(1.0);
    let hotspot_score = (hotspot_files as f64 / (file_count as f64).max(1.0)).min(1.0);
    let churn_score = (total_churn as f64 / 20.0).min(1.0);

    let risk_score = (W_BLAST * blast_score
        + W_COMPLEXITY * complexity_score
        + W_HOTSPOT * hotspot_score
        + W_CHURN * churn_score)
        .min(1.0);

    // Recommended reads: affected files ranked by blast_radius
    let recommended_reads: Vec<_> = affected_files_full
        .iter()
        .take(MAX_READS)
        .map(|(path, blast)| json!({ "path": path, "blast_radius": blast }))
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
        },
        "recommended_reads": recommended_reads,
    }))
}
