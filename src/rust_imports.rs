use std::collections::HashMap;
use std::path::Path;

use tracing::debug;

use crate::db::Db;
use crate::error::Result;

pub fn resolve_rust_imports(db: &Db, workspace_root: &Path) -> Result<usize> {
    let crate_name = read_crate_name(workspace_root);

    let unresolved = db.unresolved_rust_imports()?;
    if unresolved.is_empty() {
        return Ok(0);
    }

    let all_files = db.all_files()?;
    let path_to_id: HashMap<&str, i64> =
        all_files.iter().map(|f| (f.path.as_str(), f.id)).collect();
    let id_to_path: HashMap<i64, &str> =
        all_files.iter().map(|f| (f.id, f.path.as_str())).collect();

    let mut resolved_count = 0usize;
    for (import_id, file_id, imported_path) in &unresolved {
        let file_path = match id_to_path.get(file_id) {
            Some(p) => *p,
            None => continue,
        };
        let segments =
            match normalize_to_crate_segments(imported_path, file_path, crate_name.as_deref()) {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
        if let Some(target_id) = resolve_segments(&segments, &path_to_id)
            && target_id != *file_id
        {
            db.update_import_resolved_file_id(*import_id, target_id)?;
            resolved_count += 1;
        }
    }

    if resolved_count > 0 {
        debug!(
            total = unresolved.len(),
            resolved = resolved_count,
            "resolved Rust import edges"
        );
    }
    Ok(resolved_count)
}

fn read_crate_name(workspace_root: &Path) -> Option<String> {
    let cargo = std::fs::read_to_string(workspace_root.join("Cargo.toml")).ok()?;
    for line in cargo.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let val = rest.trim().trim_matches('"').trim_matches('\'');
                if !val.is_empty() {
                    return Some(val.replace('-', "_"));
                }
            }
        }
    }
    None
}

/// Convert an import path into module segments relative to the crate root.
/// Returns `None` for external crate imports.
fn normalize_to_crate_segments(
    imported_path: &str,
    importing_file: &str,
    crate_name: Option<&str>,
) -> Option<Vec<String>> {
    let path = imported_path.strip_suffix("::*").unwrap_or(imported_path);

    if let Some(rest) = path.strip_prefix("crate::") {
        return Some(rest.split("::").map(String::from).collect());
    }

    if let Some(name) = crate_name {
        let prefix = format!("{name}::");
        if let Some(rest) = path.strip_prefix(&prefix) {
            return Some(rest.split("::").map(String::from).collect());
        }
        if path == name {
            return Some(vec![]);
        }
    }

    if path == "super" || path.starts_with("super::") {
        let rest = path.strip_prefix("super::").unwrap_or("");
        let mut parent = file_to_module_segments(importing_file);
        if parent.is_empty() {
            return None;
        }
        parent.pop();
        if !rest.is_empty() {
            parent.extend(rest.split("::").map(String::from));
        }
        if parent.is_empty() {
            return None;
        }
        return Some(parent);
    }

    if path == "self" || path.starts_with("self::") {
        let rest = path.strip_prefix("self::").unwrap_or("");
        let mut segs = file_to_module_segments(importing_file);
        if !rest.is_empty() {
            segs.extend(rest.split("::").map(String::from));
        }
        return Some(segs);
    }

    None
}

/// Derive module segments from a file path.
/// `src/db/conventions.rs` → `["db", "conventions"]`
/// `src/db/mod.rs` → `["db"]`
/// `src/lib.rs` → `[]`
fn file_to_module_segments(file_path: &str) -> Vec<String> {
    let stripped = file_path.strip_prefix("src/").unwrap_or(file_path);

    let without_ext = stripped.strip_suffix(".rs").unwrap_or(stripped);

    let parts: Vec<&str> = without_ext.split('/').collect();
    parts
        .into_iter()
        .filter(|p| *p != "lib" && *p != "mod" && *p != "main")
        .map(String::from)
        .collect()
}

/// Try to resolve module segments to a file ID.
/// Tries longest match first: `["db", "conventions", "ConventionRow"]`
/// → `src/db/conventions/ConventionRow.rs` (no)
/// → `src/db/conventions.rs` (yes!)
fn resolve_segments(segments: &[String], path_to_id: &HashMap<&str, i64>) -> Option<i64> {
    for depth in (1..=segments.len()).rev() {
        let joined = segments[..depth]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("/");

        let file_path = format!("src/{joined}.rs");
        if let Some(&id) = path_to_id.get(file_path.as_str()) {
            return Some(id);
        }

        let mod_path = format!("src/{joined}/mod.rs");
        if let Some(&id) = path_to_id.get(mod_path.as_str()) {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_prefix() {
        let segs = normalize_to_crate_segments("crate::db::Db", "src/tools/orient.rs", None);
        assert_eq!(segs.unwrap(), vec!["db", "Db"]);
    }

    #[test]
    fn crate_name_prefix() {
        let segs = normalize_to_crate_segments("sutra::db::Db", "tests/foo.rs", Some("sutra"));
        assert_eq!(segs.unwrap(), vec!["db", "Db"]);
    }

    #[test]
    fn super_prefix() {
        let segs =
            normalize_to_crate_segments("super::scoring::Signal", "src/tools/review.rs", None);
        assert_eq!(segs.unwrap(), vec!["tools", "scoring", "Signal"]);
    }

    #[test]
    fn super_from_mod_rs() {
        let segs = normalize_to_crate_segments("super::workspace", "src/tools/mod.rs", None);
        assert_eq!(segs.unwrap(), vec!["workspace"]);
    }

    #[test]
    fn super_glob() {
        let segs = normalize_to_crate_segments("super::*", "src/db/conventions.rs", None);
        assert_eq!(segs.unwrap(), vec!["db"]);
    }

    #[test]
    fn self_prefix() {
        let segs = normalize_to_crate_segments("self::engine", "src/constraints/mod.rs", None);
        assert_eq!(segs.unwrap(), vec!["constraints", "engine"]);
    }

    #[test]
    fn external_crate_returns_none() {
        let segs =
            normalize_to_crate_segments("std::collections::HashMap", "src/tools/orient.rs", None);
        assert!(segs.is_none());
    }

    #[test]
    fn file_to_module_basic() {
        assert_eq!(
            file_to_module_segments("src/db/conventions.rs"),
            vec!["db", "conventions"]
        );
        assert_eq!(file_to_module_segments("src/db/mod.rs"), vec!["db"]);
        assert_eq!(file_to_module_segments("src/error.rs"), vec!["error"]);
        let empty: Vec<String> = vec![];
        assert_eq!(file_to_module_segments("src/lib.rs"), empty);
    }

    #[test]
    fn resolve_prefers_deepest_match() {
        let mut path_to_id = HashMap::new();
        path_to_id.insert("src/db.rs", 1);
        path_to_id.insert("src/db/conventions.rs", 2);

        let segs: Vec<String> = vec!["db", "conventions", "ConventionRow"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(resolve_segments(&segs, &path_to_id), Some(2));
    }

    #[test]
    fn resolve_falls_back_to_mod_rs() {
        let mut path_to_id = HashMap::new();
        path_to_id.insert("src/conventions/mod.rs", 3);

        let segs: Vec<String> = vec!["conventions", "engine", "FcaEngine"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(resolve_segments(&segs, &path_to_id), Some(3));
    }
}
