use serde_json::json;

use crate::config::Config;
use crate::db::Db;
use crate::error::Result;
use crate::pipeline;
use crate::workspace::WorkspaceEntry;

pub async fn handle(ws: &WorkspaceEntry, db: &Db, config: &Config) -> Result<serde_json::Value> {
    let snapshot = pipeline::parse_workspace(ws, db, config).await?;

    Ok(json!({
        "workspace": ws.id,
        "files_parsed": snapshot.files_parsed,
        "symbols_extracted": snapshot.symbols_extracted,
        "refs_extracted": snapshot.refs_extracted,
        "parse_errors": snapshot.parse_errors,
        "unresolved_refs": snapshot.unresolved_count,
        "skipped_refs": snapshot.skipped_count,
        "duration_ms": snapshot.duration_ms,
    }))
}
