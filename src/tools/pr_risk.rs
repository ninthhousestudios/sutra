use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::git;
use crate::tools::change_signals::{self, ChurnMap};
use crate::tools::scoring::{self, Signal};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PrRiskArgs {
    pub workspace: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
}

const W_BLAST: f64 = 0.35;
const W_COMPLEXITY: f64 = 0.25;
const W_CHURN: f64 = 0.20;
const W_VOLUME: f64 = 0.20;

const TOP_N: usize = 10;

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    base: Option<&str>,
    head: Option<&str>,
) -> Result<serde_json::Value> {
    let base = base.unwrap_or("HEAD~1");
    let head = head.unwrap_or("HEAD");

    let changed_paths = git::git_diff_files(workspace_root, base, head)?;
    let churn_counts = git::git_churn(workspace_root, change_signals::CHURN_WINDOW_DAYS)?;
    let churn = ChurnMap {
        counts: churn_counts,
        window_days: change_signals::CHURN_WINDOW_DAYS,
    };

    let mut result = compute(db, &changed_paths, &churn)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("base".into(), json!(base));
        obj.insert("head".into(), json!(head));
        obj.insert(
            "churn_window_days".into(),
            json!(change_signals::CHURN_WINDOW_DAYS),
        );
    }
    Ok(result)
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

    let signals = change_signals::gather(db, changed_paths, churn)?;

    let mut symbol_risks: Vec<(String, f64, i64, i64)> = Vec::new();
    for f in &signals.per_file {
        if !f.indexed {
            continue;
        }
        for s in &f.symbols {
            let blast_norm = scoring::normalize(f.blast_radius as f64, change_signals::BLAST_NORM);
            let cog_norm = scoring::normalize(s.cognitive as f64, change_signals::COMPLEXITY_NORM);
            let sym_risk = blast_norm * 0.6 + cog_norm * 0.4;
            symbol_risks.push((
                s.qualified_name.clone(),
                sym_risk,
                f.blast_radius,
                s.cognitive,
            ));
        }
    }

    let file_count = changed_paths.len();

    let blast_score = scoring::normalize(signals.total_blast as f64, change_signals::BLAST_NORM);
    let complexity_score = scoring::normalize(
        signals.max_cognitive as f64,
        change_signals::COMPLEXITY_NORM,
    );
    let churn_score = scoring::normalize(signals.total_churn as f64, change_signals::CHURN_NORM);
    let volume_score = scoring::normalize(file_count as f64, 25.0);

    let composite = scoring::weighted_score(&[
        Signal {
            weight: W_BLAST,
            score: blast_score,
        },
        Signal {
            weight: W_COMPLEXITY,
            score: complexity_score,
        },
        Signal {
            weight: W_CHURN,
            score: churn_score,
        },
        Signal {
            weight: W_VOLUME,
            score: volume_score,
        },
    ]);

    symbol_risks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    symbol_risks.truncate(TOP_N);

    let top_symbols: Vec<_> = symbol_risks
        .iter()
        .map(|(name, risk, blast, cog)| {
            json!({
                "symbol": name,
                "risk_score": scoring::round3(*risk),
                "blast_radius": blast,
                "cognitive": cog,
            })
        })
        .collect();

    Ok(json!({
        "composite_score": scoring::round3(composite),
        "signals": {
            "blast_radius": { "score": scoring::round3(blast_score), "raw": signals.total_blast },
            "complexity":   { "score": scoring::round3(complexity_score), "raw": signals.max_cognitive },
            "churn":        { "score": scoring::round3(churn_score), "raw": signals.total_churn },
            "volume":       { "score": scoring::round3(volume_score), "raw": file_count },
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
