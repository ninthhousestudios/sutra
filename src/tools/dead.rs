use serde_json::json;

use crate::db::Db;
use crate::error::Result;

pub fn handle(db: &Db, path_prefix: Option<&str>, include_pub: bool) -> Result<serde_json::Value> {
    let dead_symbols = db.find_dead_symbols(include_pub, path_prefix)?;
    let unreachable_files = db.find_unreachable_files(path_prefix)?;

    let sym_items: Vec<_> = dead_symbols
        .iter()
        .map(|(qn, path, kind, line, visibility)| {
            json!({
                "symbol": qn,
                "file": path,
                "kind": kind,
                "line": line,
                "visibility": visibility,
            })
        })
        .collect();

    let file_items: Vec<_> = unreachable_files
        .iter()
        .map(|(path, line_count)| {
            json!({
                "path": path,
                "line_count": line_count,
            })
        })
        .collect();

    Ok(json!({
        "dead_symbols": sym_items,
        "dead_symbol_count": sym_items.len(),
        "unreachable_files": file_items,
        "unreachable_file_count": file_items.len(),
    }))
}
