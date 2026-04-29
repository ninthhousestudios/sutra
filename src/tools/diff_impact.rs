use std::collections::HashSet;
use std::path::Path;

use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::git;

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    base: Option<&str>,
    head: Option<&str>,
) -> Result<serde_json::Value> {
    let base = base.unwrap_or("HEAD~1");
    let head = head.unwrap_or("HEAD");

    let changed_paths = git::git_diff_files(workspace_root, base, head)?;

    let mut changed_files: Vec<serde_json::Value> = Vec::new();
    let mut all_symbol_ids: HashSet<i64> = HashSet::new();
    let mut max_cognitive: Option<i64> = None;
    let mut max_cognitive_symbol: Option<String> = None;
    let mut total_blast: i64 = 0;

    for path in &changed_paths {
        if let Some(file) = db.file_by_path(path)? {
            total_blast += file.blast_radius;
            let syms = db.find_symbols_by_file(file.id)?;
            for s in &syms {
                all_symbol_ids.insert(s.id);
                if let Some(c) = s.cognitive
                    && max_cognitive.is_none_or(|prev| c > prev)
                {
                    max_cognitive = Some(c);
                    max_cognitive_symbol = Some(s.qualified_name.clone());
                }
            }
            let symbols: Vec<_> = syms.into_iter().map(|s| s.qualified_name).collect();
            changed_files.push(json!({
                "path": path,
                "symbols": symbols,
            }));
        } else {
            changed_files.push(json!({
                "path": path,
                "symbols": Vec::<String>::new(),
            }));
        }
    }

    let symbol_ids: Vec<i64> = all_symbol_ids.into_iter().collect();
    let affected_file_ids = if symbol_ids.is_empty() {
        Vec::new()
    } else {
        db.find_files_referencing_symbols(&symbol_ids)?
    };

    let affected_files: Vec<String> = affected_file_ids
        .into_iter()
        .filter_map(|fid| db.file_by_id(fid).ok().flatten().map(|f| f.path))
        .filter(|p| !changed_paths.contains(p))
        .collect();

    let impact_count = affected_files.len();

    let mut verdict_reasons: Vec<String> = Vec::new();

    let has_complexity = max_cognitive.is_some();
    let max_cog = max_cognitive.unwrap_or(0);

    if impact_count >= 30 {
        verdict_reasons.push(format!("{impact_count} affected files (threshold: 30)"));
    } else if impact_count >= 10 {
        verdict_reasons.push(format!("{impact_count} affected files (threshold: 10)"));
    }
    if max_cog >= 25 {
        verdict_reasons.push(format!(
            "max cognitive complexity {} in {} (threshold: 25)",
            max_cog,
            max_cognitive_symbol.as_deref().unwrap_or("?")
        ));
    } else if max_cog >= 15 {
        verdict_reasons.push(format!(
            "max cognitive complexity {} in {} (threshold: 15)",
            max_cog,
            max_cognitive_symbol.as_deref().unwrap_or("?")
        ));
    }
    if total_blast >= 50 {
        verdict_reasons.push(format!("total blast radius {total_blast} (threshold: 50)"));
    } else if total_blast >= 20 {
        verdict_reasons.push(format!("total blast radius {total_blast} (threshold: 20)"));
    }
    if !has_complexity {
        verdict_reasons.push("complexity data unavailable — reparse to populate".to_string());
    }

    let verdict = if impact_count >= 30 || max_cog >= 25 || total_blast >= 50 {
        "fail"
    } else if impact_count >= 10 || max_cog >= 15 || total_blast >= 20 {
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
            "max_cognitive_symbol": max_cognitive_symbol,
            "total_blast_radius": total_blast,
        },
    }))
}
