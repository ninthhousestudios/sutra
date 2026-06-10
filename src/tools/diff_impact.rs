use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::git;
use crate::tools::change_signals::{self, ChurnMap};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiffImpactArgs {
    pub workspace: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    base: Option<&str>,
    head: Option<&str>,
) -> Result<serde_json::Value> {
    let base = base.unwrap_or("HEAD~1");
    let head = head.unwrap_or("HEAD");

    let changed_paths = git::git_diff_files(workspace_root, base, head)?;
    let signals = change_signals::gather(db, &changed_paths, &ChurnMap::default(), true)?;

    let changed_files: Vec<_> = signals
        .per_file
        .iter()
        .map(|f| {
            let symbols: Vec<&str> = f
                .symbols
                .iter()
                .map(|s| s.qualified_name.as_str())
                .collect();
            json!({ "path": f.path, "symbols": symbols })
        })
        .collect();

    let affected_files: Vec<&str> = signals
        .affected_files
        .iter()
        .map(|a| a.path.as_str())
        .collect();

    let impact_count = affected_files.len();
    let max_cog = signals.max_cognitive.unwrap_or(0);

    let mut verdict_reasons: Vec<String> = Vec::new();

    if impact_count >= 30 {
        verdict_reasons.push(format!("{impact_count} affected files (threshold: 30)"));
    } else if impact_count >= 10 {
        verdict_reasons.push(format!("{impact_count} affected files (threshold: 10)"));
    }
    if max_cog >= 25 {
        verdict_reasons.push(format!(
            "max cognitive complexity {} in {} (threshold: 25)",
            max_cog,
            signals.max_cognitive_symbol.as_deref().unwrap_or("?")
        ));
    } else if max_cog >= 15 {
        verdict_reasons.push(format!(
            "max cognitive complexity {} in {} (threshold: 15)",
            max_cog,
            signals.max_cognitive_symbol.as_deref().unwrap_or("?")
        ));
    }
    if signals.total_blast >= 50 {
        verdict_reasons.push(format!(
            "total blast radius {} (threshold: 50)",
            signals.total_blast
        ));
    } else if signals.total_blast >= 20 {
        verdict_reasons.push(format!(
            "total blast radius {} (threshold: 20)",
            signals.total_blast
        ));
    }
    if signals.max_cognitive.is_none() {
        verdict_reasons.push("complexity data unavailable — reparse to populate".to_string());
    }

    let verdict = if impact_count >= 30 || max_cog >= 25 || signals.total_blast >= 50 {
        "fail"
    } else if impact_count >= 10 || max_cog >= 15 || signals.total_blast >= 20 {
        "warn"
    } else {
        "pass"
    };

    Ok(json!({
        "base": base,
        "head": head,
        "changed_files": changed_files,
        "affected_files": affected_files,
        "impact_count": impact_count,
        "verdict": verdict,
        "verdict_reasons": verdict_reasons,
        "risk_metrics": {
            "affected_file_count": impact_count,
            "max_cognitive": max_cog,
            "max_cognitive_symbol": signals.max_cognitive_symbol,
            "total_blast_radius": signals.total_blast,
        },
    }))
}
