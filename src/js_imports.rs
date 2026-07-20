use std::collections::HashMap;
use std::path::Path;

use tracing::debug;

use crate::db::Db;
use crate::error::Result;

const EXTENSIONS: &[&str] = &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"];
const INDEX_FILES: &[&str] = &[
    "index.ts",
    "index.tsx",
    "index.js",
    "index.jsx",
    "index.mjs",
    "index.cjs",
];

pub fn resolve_js_imports(db: &Db, _workspace_root: &Path) -> Result<usize> {
    let unresolved = db.unresolved_js_ts_imports()?;
    if unresolved.is_empty() {
        return Ok(0);
    }

    let all_files = db.all_files()?;
    let path_to_id: HashMap<&str, i64> = all_files.iter().map(|f| (&*f.path, f.id)).collect();
    let id_to_path: HashMap<i64, &str> = all_files.iter().map(|f| (f.id, &*f.path)).collect();

    let mut updates = Vec::new();
    for (import_id, file_id, raw_path, _kind) in &unresolved {
        if !is_relative(raw_path) {
            continue;
        }

        let resolved = resolve_relative(raw_path, *file_id, &id_to_path, &path_to_id);
        if let Some(target_id) = resolved {
            if target_id != *file_id {
                updates.push((*import_id, target_id));
            }
        }
    }

    let resolved_count = updates.len();
    db.batch_update_import_resolved_file_ids(&updates)?;

    if resolved_count > 0 {
        debug!(
            total = unresolved.len(),
            resolved = resolved_count,
            "resolved JS/TS import edges"
        );
    }
    Ok(resolved_count)
}

fn is_relative(path: &str) -> bool {
    path.starts_with("./") || path.starts_with("../")
}

fn resolve_relative(
    raw_path: &str,
    file_id: i64,
    id_to_path: &HashMap<i64, &str>,
    path_to_id: &HashMap<&str, i64>,
) -> Option<i64> {
    let file_path = *id_to_path.get(&file_id)?;
    let dir = match file_path.rsplit_once('/') {
        Some((d, _)) => d,
        None => "",
    };

    let resolved = resolve_path_segments(raw_path, dir)?;
    let candidate = resolved.as_str();

    if has_js_extension(candidate) {
        return path_to_id.get(candidate).copied();
    }

    for ext in EXTENSIONS {
        let with_ext = format!("{candidate}{ext}");
        if let Some(&id) = path_to_id.get(with_ext.as_str()) {
            return Some(id);
        }
    }

    for index in INDEX_FILES {
        let index_path = if candidate.is_empty() {
            index.to_string()
        } else {
            format!("{candidate}/{index}")
        };
        if let Some(&id) = path_to_id.get(index_path.as_str()) {
            return Some(id);
        }
    }

    None
}

fn resolve_path_segments(raw_path: &str, dir: &str) -> Option<String> {
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };

    for segment in raw_path.split('/') {
        match segment {
            "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return None;
                }
            }
            s => parts.push(s),
        }
    }

    Some(parts.join("/"))
}

fn has_js_extension(path: &str) -> bool {
    matches!(
        path.rsplit_once('.'),
        Some((
            _,
            "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "mts" | "cts"
        ))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_path_segments() {
        assert_eq!(
            resolve_path_segments("./utils", "src"),
            Some("src/utils".into())
        );
        assert_eq!(
            resolve_path_segments("../lib/foo", "src/app"),
            Some("src/lib/foo".into())
        );
        assert_eq!(resolve_path_segments("./bar", ""), Some("bar".into()));
        assert_eq!(
            resolve_path_segments("../../top", "a/b/c"),
            Some("a/top".into())
        );
    }

    #[test]
    fn test_resolve_path_segments_above_root() {
        assert_eq!(resolve_path_segments("../../utils", "src"), None);
        assert_eq!(resolve_path_segments("../foo", ""), None);
    }

    #[test]
    fn test_has_js_extension() {
        assert!(has_js_extension("foo.js"));
        assert!(has_js_extension("foo.tsx"));
        assert!(has_js_extension("foo.mjs"));
        assert!(has_js_extension("foo.mts"));
        assert!(has_js_extension("foo.cts"));
        assert!(!has_js_extension("foo"));
        assert!(!has_js_extension("foo.py"));
    }

    #[test]
    fn test_is_relative() {
        assert!(is_relative("./foo"));
        assert!(is_relative("../bar"));
        assert!(!is_relative("react"));
        assert!(!is_relative("@angular/core"));
    }
}
