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

    for path in &changed_paths {
        let symbols = if let Some(file) = db.file_by_path(path)? {
            let syms = db.find_symbols_by_file(file.id)?;
            for s in &syms {
                all_symbol_ids.insert(s.id);
            }
            syms.into_iter().map(|s| s.qualified_name).collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        changed_files.push(json!({
            "path": path,
            "symbols": symbols,
        }));
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

    Ok(json!({
        "base": base,
        "head": head,
        "changed_files": changed_files,
        "affected_files": affected_files,
        "impact_count": impact_count,
    }))
}
