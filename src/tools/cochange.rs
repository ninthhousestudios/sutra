use std::path::Path;

use serde_json::json;

use crate::db::Db;
use crate::error::Result;
use crate::git;

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    path: &str,
    window_days: Option<u32>,
) -> Result<serde_json::Value> {
    let window_days = window_days.unwrap_or(90);

    let cochanged = git::git_cochange_files(workspace_root, path, window_days)?;

    let entries: Vec<serde_json::Value> = cochanged
        .into_iter()
        .map(|(co_path, count)| {
            let in_index = db.file_by_path(&co_path).ok().flatten().is_some();
            json!({
                "path": co_path,
                "count": count,
                "in_index": in_index,
            })
        })
        .collect();

    Ok(json!({
        "path": path,
        "window_days": window_days,
        "cochanged": entries,
    }))
}
