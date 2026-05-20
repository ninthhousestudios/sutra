use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::json;
use std::collections::HashMap;

use crate::config::Config;
use crate::db::Db;
use crate::error::Result;
use crate::pipeline::ParseCoordinator;
use crate::workspace::WorkspaceEntry;

pub fn handle(
    workspaces: &[WorkspaceEntry],
    db_cache: &Mutex<HashMap<String, Arc<Db>>>,
    config: &Config,
    parse_coord: &ParseCoordinator,
) -> Result<serde_json::Value> {
    let mut ws_summaries = Vec::new();
    let mut overall_ok = true;

    for ws in workspaces {
        let db = super::get_or_open_db(db_cache, &ws.id, &config.db_dir)?;
        let files = db.all_files()?;
        let sym_counts = db.symbol_counts_by_file()?;
        let total_symbols: i64 = sym_counts.values().sum();
        let parse_errors = files.iter().filter(|f| !f.parsed_ok).count();
        let last_parse = db.last_parse_time()?;

        if parse_errors > 0 || last_parse.is_none() {
            overall_ok = false;
        }

        let mut entry = json!({
            "workspace": ws.id,
            "root": ws.root.display().to_string(),
            "languages": ws.languages,
            "files": files.len(),
            "symbols": total_symbols,
            "parse_errors": parse_errors,
            "last_parse": last_parse,
        });
        if parse_coord.is_locked(&ws.id) {
            entry["parsing_in_progress"] = serde_json::Value::Bool(true);
        }
        ws_summaries.push(entry);
    }

    Ok(json!({
        "status": if overall_ok { "ok" } else { "degraded" },
        "workspaces": ws_summaries,
    }))
}
