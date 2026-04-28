use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashMap;

use crate::config::Config;
use crate::db::Db;
use crate::error::Result;
use crate::workspace::WorkspaceEntry;

pub fn handle(
    workspaces: &[WorkspaceEntry],
    db_cache: &Mutex<HashMap<String, Arc<Db>>>,
    config: &Config,
) -> Result<serde_json::Value> {
    let mut ws_summaries = Vec::new();
    let mut overall_ok = true;

    for ws in workspaces {
        let db = super::get_or_open_db(db_cache, &ws.id, &config.db_dir)?;
        let files = db.all_files()?;
        let total_symbols: usize = files
            .iter()
            .map(|f| db.find_symbols_by_file(f.id).map(|s| s.len()).unwrap_or(0))
            .sum();
        let parse_errors = files.iter().filter(|f| !f.parsed_ok).count();
        let last_parse = db.last_parse_time()?;

        if parse_errors > 0 || last_parse.is_none() {
            overall_ok = false;
        }

        ws_summaries.push(json!({
            "workspace": ws.id,
            "root": ws.root.display().to_string(),
            "languages": ws.languages,
            "files": files.len(),
            "symbols": total_symbols,
            "parse_errors": parse_errors,
            "last_parse": last_parse,
        }));
    }

    Ok(json!({
        "status": if overall_ok { "ok" } else { "degraded" },
        "workspaces": ws_summaries,
    }))
}
