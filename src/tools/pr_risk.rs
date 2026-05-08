use std::collections::HashMap;
use std::path::Path;

use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::git;

// ── Weight rationale ──────────────────────────────────────────────────
//
// blast_radius (0.35): strongest predictor — changes rippling to many
//   downstream files carry the most review risk.
// complexity (0.25): complex code is harder to review and more likely
//   to hide bugs. Uses max cognitive complexity across changed symbols.
// churn (0.20): files changed frequently in the recent past suggest
//   instability or poor encapsulation.
// volume (0.20): sheer number of changed files/symbols increases the
//   surface area a reviewer must cover.

const W_BLAST: f64 = 0.35;
const W_COMPLEXITY: f64 = 0.25;
const W_CHURN: f64 = 0.20;
const W_VOLUME: f64 = 0.20;

const TOP_N: usize = 10;
const CHURN_WINDOW_DAYS: u32 = 90;

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    base: Option<&str>,
    head: Option<&str>,
) -> Result<serde_json::Value> {
    let base = base.unwrap_or("HEAD~1");
    let head = head.unwrap_or("HEAD");

    let changed_paths = git::git_diff_files(workspace_root, base, head)?;
    let churn_counts = git::git_churn(workspace_root, CHURN_WINDOW_DAYS)?;
    let churn = ChurnMap {
        counts: churn_counts,
        window_days: CHURN_WINDOW_DAYS,
    };

    let mut result = compute(db, &changed_paths, &churn)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("base".into(), json!(base));
        obj.insert("head".into(), json!(head));
        obj.insert("churn_window_days".into(), json!(CHURN_WINDOW_DAYS));
    }
    Ok(result)
}

/// Per-file churn counts (path → commit count). Injected so the core
/// scoring logic is testable without git.
#[derive(Default)]
pub struct ChurnMap {
    pub counts: HashMap<String, u32>,
    pub window_days: u32,
}

pub fn compute(db: &Db, changed_paths: &[String], churn: &ChurnMap) -> Result<serde_json::Value> {
    if changed_paths.is_empty() {
        return Ok(json!({
            "composite_score": 0.0,
            "signals": {
                "blast_radius": { "score": 0.0, "raw": 0 },
                "complexity":   { "score": 0.0, "raw": 0 },
                "churn":        { "score": 0.0, "raw": 0 },
                "volume":       { "score": 0.0, "raw": 0 },
            },
            "riskiest_symbols": [],
            "weights": weights_doc(),
        }));
    }

    let mut total_blast: i64 = 0;
    let mut max_cognitive: i64 = 0;
    let mut total_churn: u32 = 0;
    let mut symbol_risks: Vec<(String, f64, i64, i64)> = Vec::new(); // (name, risk, blast, cog)

    for path in changed_paths {
        if let Some(file) = db.file_by_path(path)? {
            total_blast += file.blast_radius;
            let file_churn = churn.counts.get(path).copied().unwrap_or(0);
            total_churn += file_churn;

            let syms = db.find_symbols_by_file(file.id)?;
            for s in &syms {
                let cog = s.cognitive.unwrap_or(0);
                if cog > max_cognitive {
                    max_cognitive = cog;
                }
                let blast_norm = (file.blast_radius as f64 / 50.0).min(1.0);
                let cog_norm = (cog as f64 / 30.0).min(1.0);
                let sym_risk = blast_norm * 0.6 + cog_norm * 0.4;
                symbol_risks.push((s.qualified_name.clone(), sym_risk, file.blast_radius, cog));
            }
        }
    }

    let file_count = changed_paths.len();

    let blast_score = (total_blast as f64 / 50.0).min(1.0);
    let complexity_score = (max_cognitive as f64 / 30.0).min(1.0);
    let churn_score = (total_churn as f64 / 20.0).min(1.0);
    let volume_score = (file_count as f64 / 25.0).min(1.0);

    let composite = (W_BLAST * blast_score
        + W_COMPLEXITY * complexity_score
        + W_CHURN * churn_score
        + W_VOLUME * volume_score)
        .min(1.0);

    symbol_risks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    symbol_risks.truncate(TOP_N);

    let top_symbols: Vec<_> = symbol_risks
        .iter()
        .map(|(name, risk, blast, cog)| {
            json!({
                "symbol": name,
                "risk_score": (*risk * 1000.0).round() / 1000.0,
                "blast_radius": blast,
                "cognitive": cog,
            })
        })
        .collect();

    Ok(json!({
        "composite_score": (composite * 1000.0).round() / 1000.0,
        "signals": {
            "blast_radius": { "score": (blast_score * 1000.0).round() / 1000.0, "raw": total_blast },
            "complexity":   { "score": (complexity_score * 1000.0).round() / 1000.0, "raw": max_cognitive },
            "churn":        { "score": (churn_score * 1000.0).round() / 1000.0, "raw": total_churn },
            "volume":       { "score": (volume_score * 1000.0).round() / 1000.0, "raw": file_count },
        },
        "riskiest_symbols": top_symbols,
        "weights": weights_doc(),
    }))
}

fn weights_doc() -> serde_json::Value {
    json!({
        "blast_radius": { "weight": W_BLAST, "rationale": "changes rippling to many downstream files carry the most review risk" },
        "complexity":   { "weight": W_COMPLEXITY, "rationale": "complex code is harder to review and more likely to hide bugs" },
        "churn":        { "weight": W_CHURN, "rationale": "frequently-changed files suggest instability or poor encapsulation" },
        "volume":       { "weight": W_VOLUME, "rationale": "more changed files/symbols increases reviewer surface area" },
    })
}
