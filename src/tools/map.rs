use serde_json::json;

use crate::db::Db;
use crate::error::Result;

pub fn handle(db: &Db, path_prefix: Option<&str>, limit: Option<i64>) -> Result<serde_json::Value> {
    let limit = limit.unwrap_or(50);
    let files = db.all_files()?;
    let sym_counts = db.symbol_counts_by_file()?;

    let mut entries: Vec<_> = files
        .into_iter()
        .filter(|f| match path_prefix {
            Some(prefix) => f.path.starts_with(prefix),
            None => true,
        })
        .map(|f| {
            let symbol_count = sym_counts.get(&f.id).copied().unwrap_or(0);
            let importance = symbol_count + f.fan_in_files * 2 + f.blast_radius;
            (f, symbol_count, importance)
        })
        .collect();

    entries.sort_by_key(|e| std::cmp::Reverse(e.2));
    entries.truncate(limit as usize);

    let items: Vec<_> = entries
        .iter()
        .map(|(f, sym_count, importance)| {
            json!({
                "path": f.path,
                "language": f.language,
                "line_count": f.line_count,
                "symbols": sym_count,
                "fan_in_files": f.fan_in_files,
                "blast_radius": f.blast_radius,
                "importance": importance,
            })
        })
        .collect();

    Ok(json!({ "files": items, "total": items.len() }))
}
