use serde_json::json;

use crate::db::Db;
use crate::error::Result;

pub fn handle(
    db: &Db,
    name: &str,
    kind: Option<&str>,
    limit: Option<i64>,
) -> Result<serde_json::Value> {
    let limit = limit.unwrap_or(10);
    let results = db.find_symbols_by_name(name, kind, limit)?;

    let items: Vec<_> = results
        .iter()
        .map(|s| {
            let file_path = db.file_by_id(s.file_id).ok().flatten().map(|f| f.path);
            json!({
                "id": s.id,
                "qualified_name": s.qualified_name,
                "short_name": s.short_name,
                "kind": s.kind,
                "file": file_path,
                "start_line": s.start_line,
                "end_line": s.end_line,
                "signature": s.signature,
                "visibility": s.visibility,
            })
        })
        .collect();

    Ok(json!({ "matches": items, "total": items.len() }))
}
