use std::sync::atomic::AtomicBool;

use serde_json::json;

use crate::config::Config;
use crate::db::Db;
use crate::error::Result;
use crate::parser::adapter::LanguageRegistry;
use crate::pipeline;
use crate::workspace::WorkspaceEntry;

pub fn handle(
    ws: &WorkspaceEntry,
    db: &Db,
    config: &Config,
    cancel: &AtomicBool,
    registry: &LanguageRegistry,
) -> Result<serde_json::Value> {
    let snapshot = pipeline::parse_workspace(ws, db, config, cancel, registry)?;

    Ok(json!({
        "workspace": ws.id,
        "files_walked": snapshot.files_walked,
        "files_parsed": snapshot.files_parsed,
        "symbols_extracted": snapshot.symbols_extracted,
        "refs_extracted": snapshot.refs_extracted,
        "parse_errors": snapshot.parse_errors,
        "resolved_refs": snapshot.resolved_count,
        "unresolved_refs": snapshot.unresolved_count,
        "skipped_refs": snapshot.skipped_count,
        "duration_ms": snapshot.duration_ms,
    }))
}
