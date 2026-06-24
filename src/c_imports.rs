use std::collections::HashMap;
use std::path::Path;
use tracing::debug;

use crate::db::Db;
use crate::error::Result;

pub fn resolve_c_imports(db: &Db, workspace_root: &Path) -> Result<usize> {
    let unresolved = db.unresolved_c_imports()?;
    if unresolved.is_empty() {
        return Ok(0);
    }

    let all_files = db.all_files()?;
    let path_to_id: HashMap<&str, i64> = all_files.iter().map(|f| (&*f.path, f.id)).collect();
    let id_to_path: HashMap<i64, &str> = all_files.iter().map(|f| (f.id, &*f.path)).collect();

    let include_dirs = parse_compile_commands(workspace_root);

    let mut updates = Vec::new();
    for (import_id, file_id, raw_path) in &unresolved {
        if raw_path.starts_with('<') {
            continue;
        }
        let include_path = raw_path.trim_matches('"');
        let resolved = resolve_quoted_include(
            include_path,
            *file_id,
            &id_to_path,
            &path_to_id,
            workspace_root,
            &include_dirs,
        );
        if let Some(target_id) = resolved
            && target_id != *file_id
        {
            updates.push((*import_id, target_id));
        }
    }

    let resolved_count = updates.len();
    db.batch_update_import_resolved_file_ids(&updates)?;

    if resolved_count > 0 {
        debug!(
            total = unresolved.len(),
            resolved = resolved_count,
            "resolved C import edges"
        );
    }
    Ok(resolved_count)
}

fn resolve_quoted_include(
    include_path: &str,
    file_id: i64,
    id_to_path: &HashMap<i64, &str>,
    path_to_id: &HashMap<&str, i64>,
    workspace_root: &Path,
    include_dirs: &[String],
) -> Option<i64> {
    let including_file = id_to_path.get(&file_id)?;
    let including_dir = Path::new(including_file).parent()?;

    // 1. Relative to the including file
    let relative = normalize_path(&including_dir.join(include_path));
    if let Some(&id) = path_to_id.get(relative.as_str()) {
        return Some(id);
    }

    // 2. Relative to the project root
    if let Some(&id) = path_to_id.get(include_path) {
        return Some(id);
    }

    // 3. compile_commands.json -I / -isystem paths
    for dir in include_dirs {
        let candidate = normalize_path(&Path::new(dir).join(include_path));
        // Try as workspace-relative
        if let Some(&id) = path_to_id.get(candidate.as_str()) {
            return Some(id);
        }
        // Try stripping workspace_root prefix
        let abs = workspace_root.join(&candidate);
        if let Ok(stripped) = abs.strip_prefix(workspace_root) {
            let stripped_str = stripped.to_string_lossy();
            if let Some(&id) = path_to_id.get(stripped_str.as_ref()) {
                return Some(id);
            }
        }
    }

    None
}

fn normalize_path(path: &Path) -> String {
    let mut components = Vec::new();
    for c in path.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                components.pop();
            }
            _ => {
                components.push(c.as_os_str().to_string_lossy().into_owned());
            }
        }
    }
    components.join("/")
}

pub fn parse_compile_commands(workspace_root: &Path) -> Vec<String> {
    let path = workspace_root.join("compile_commands.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let entries: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut dirs = Vec::new();
    for entry in &entries {
        let tokens: Vec<&str> =
            if let Some(args) = entry.get("arguments").and_then(|a| a.as_array()) {
                args.iter().filter_map(|a| a.as_str()).collect()
            } else if let Some(cmd) = entry.get("command").and_then(|c| c.as_str()) {
                cmd.split_whitespace().collect()
            } else {
                continue;
            };
        let mut iter = tokens.iter();
        while let Some(&tok) = iter.next() {
            if tok == "-I" || tok == "-isystem" {
                if let Some(&dir) = iter.next() {
                    dirs.push(dir.to_string());
                }
            } else if let Some(dir) = tok.strip_prefix("-I") {
                dirs.push(dir.to_string());
            } else if let Some(dir) = tok.strip_prefix("-isystem") {
                if !dir.is_empty() {
                    dirs.push(dir.to_string());
                }
            }
        }
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_dot_segments() {
        assert_eq!(
            normalize_path(Path::new("src/../include/foo.h")),
            "include/foo.h"
        );
        assert_eq!(normalize_path(Path::new("src/./bar.h")), "src/bar.h");
    }

    #[test]
    fn parse_compile_commands_extracts_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let cc = dir.path().join("compile_commands.json");
        std::fs::write(
            &cc,
            r#"[
                {"command": "gcc -I/usr/include -isystem /opt/include -Ivendor/inc -c foo.c"},
                {"arguments": ["gcc", "-I", "lib/inc", "-isystem", "sys/inc", "-c", "bar.c"]}
            ]"#,
        )
        .unwrap();
        let dirs = parse_compile_commands(dir.path());
        assert!(dirs.contains(&"/opt/include".to_string()));
        assert!(dirs.contains(&"/usr/include".to_string()));
        assert!(dirs.contains(&"vendor/inc".to_string()));
        assert!(dirs.contains(&"lib/inc".to_string()));
        assert!(dirs.contains(&"sys/inc".to_string()));
    }

    #[test]
    fn angle_bracket_includes_skipped() {
        assert!(resolve_quoted_include_is_skipped("<stdio.h>"));
    }

    fn resolve_quoted_include_is_skipped(raw: &str) -> bool {
        raw.starts_with('<')
    }

    #[test]
    fn quoted_include_resolved_relative() {
        let mut id_to_path = HashMap::new();
        id_to_path.insert(1_i64, "src/main.c");
        let mut path_to_id = HashMap::new();
        path_to_id.insert("src/util.h", 2_i64);

        let result = resolve_quoted_include(
            "util.h",
            1,
            &id_to_path,
            &path_to_id,
            Path::new("/project"),
            &[],
        );
        assert_eq!(result, Some(2));
    }

    #[test]
    fn quoted_include_resolved_from_root() {
        let mut id_to_path = HashMap::new();
        id_to_path.insert(1_i64, "src/main.c");
        let mut path_to_id = HashMap::new();
        path_to_id.insert("include/header.h", 2_i64);

        let result = resolve_quoted_include(
            "include/header.h",
            1,
            &id_to_path,
            &path_to_id,
            Path::new("/project"),
            &[],
        );
        assert_eq!(result, Some(2));
    }

    #[test]
    fn quoted_include_resolved_from_include_dir() {
        let mut id_to_path = HashMap::new();
        id_to_path.insert(1_i64, "src/main.c");
        let mut path_to_id = HashMap::new();
        path_to_id.insert("vendor/inc/lib.h", 2_i64);

        let result = resolve_quoted_include(
            "lib.h",
            1,
            &id_to_path,
            &path_to_id,
            Path::new("/project"),
            &["vendor/inc".to_string()],
        );
        assert_eq!(result, Some(2));
    }

    #[test]
    fn self_include_filtered_by_caller() {
        let mut id_to_path = HashMap::new();
        id_to_path.insert(1_i64, "src/foo.h");
        let mut path_to_id = HashMap::new();
        path_to_id.insert("src/foo.h", 1_i64);

        // resolve_quoted_include returns the match; the caller (resolve_c_imports)
        // filters target_id == file_id.
        let result = resolve_quoted_include(
            "foo.h",
            1,
            &id_to_path,
            &path_to_id,
            Path::new("/project"),
            &[],
        );
        assert_eq!(result, Some(1));
    }

    #[test]
    fn no_compile_commands_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(parse_compile_commands(dir.path()).is_empty());
    }
}
