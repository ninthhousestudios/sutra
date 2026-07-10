use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::debug;

use crate::db::Db;
use crate::error::Result;

pub struct CompileCommands {
    pub per_file: HashMap<String, Vec<String>>,
    pub all_dirs: Vec<String>,
}

impl CompileCommands {
    fn dirs_for(&self, file_path: &str) -> &[String] {
        self.per_file.get(file_path).map_or(&[], |v| v.as_slice())
    }

    fn fallback_dirs(&self) -> &[String] {
        &self.all_dirs
    }
}

pub fn resolve_c_imports(db: &Db, workspace_root: &Path) -> Result<usize> {
    let unresolved = db.unresolved_c_imports()?;
    if unresolved.is_empty() {
        return Ok(0);
    }

    let all_files = db.all_files()?;
    let path_to_id: HashMap<&str, i64> = all_files.iter().map(|f| (&*f.path, f.id)).collect();
    let id_to_path: HashMap<i64, &str> = all_files.iter().map(|f| (f.id, &*f.path)).collect();

    let compile_commands = parse_compile_commands(workspace_root);

    let mut updates = Vec::new();
    for (import_id, file_id, raw_path) in &unresolved {
        if raw_path.starts_with('<') {
            continue;
        }
        let include_path = raw_path.trim_matches('"');
        let including_path = id_to_path.get(file_id).copied();
        let dirs = including_path
            .map(|p| compile_commands.dirs_for(p))
            .and_then(|d| if d.is_empty() { None } else { Some(d) })
            .unwrap_or(compile_commands.fallback_dirs());
        let resolved = resolve_quoted_include(
            include_path,
            *file_id,
            &id_to_path,
            &path_to_id,
            workspace_root,
            dirs,
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

pub fn resolve_quoted_include(
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

pub fn parse_compile_commands(workspace_root: &Path) -> CompileCommands {
    let empty = CompileCommands {
        per_file: HashMap::new(),
        all_dirs: Vec::new(),
    };
    let path = workspace_root.join("compile_commands.json");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return empty,
    };
    let entries: Vec<serde_json::Value> = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return empty,
    };

    let mut per_file: HashMap<String, Vec<String>> = HashMap::new();

    for entry in &entries {
        let file = match entry.get("file").and_then(|f| f.as_str()) {
            Some(f) => f,
            None => continue,
        };
        let directory = entry.get("directory").and_then(|d| d.as_str());

        let file_key = normalize_entry_path(file, directory, workspace_root);

        let tokens: Vec<&str> =
            if let Some(args) = entry.get("arguments").and_then(|a| a.as_array()) {
                args.iter().filter_map(|a| a.as_str()).collect()
            } else if let Some(cmd) = entry.get("command").and_then(|c| c.as_str()) {
                cmd.split_whitespace().collect()
            } else {
                continue;
            };

        let mut dirs = Vec::new();
        let mut iter = tokens.iter();
        while let Some(&tok) = iter.next() {
            if tok == "-I" || tok == "-isystem" {
                if let Some(&dir) = iter.next() {
                    dirs.push(resolve_include_dir(dir, directory, workspace_root));
                }
            } else if let Some(dir) = tok.strip_prefix("-I") {
                dirs.push(resolve_include_dir(dir, directory, workspace_root));
            } else if let Some(dir) = tok.strip_prefix("-isystem")
                && !dir.is_empty()
            {
                dirs.push(resolve_include_dir(dir, directory, workspace_root));
            }
        }

        if !dirs.is_empty() {
            per_file.entry(file_key).or_default().extend(dirs);
        }
    }

    let mut all_dirs = Vec::new();
    let mut seen = HashSet::new();
    for dirs in per_file.values() {
        for d in dirs {
            if seen.insert(d.clone()) {
                all_dirs.push(d.clone());
            }
        }
    }

    CompileCommands { per_file, all_dirs }
}

fn normalize_entry_path(file: &str, directory: Option<&str>, workspace_root: &Path) -> String {
    let p = Path::new(file);
    if p.is_absolute() {
        return p
            .strip_prefix(workspace_root)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|_| file.to_string());
    }
    if let Some(dir) = directory {
        let abs = Path::new(dir).join(file);
        return abs
            .strip_prefix(workspace_root)
            .map(normalize_path)
            .unwrap_or_else(|_| normalize_path(&abs));
    }
    file.to_string()
}

fn resolve_include_dir(dir: &str, directory: Option<&str>, workspace_root: &Path) -> String {
    let p = Path::new(dir);
    if p.is_absolute() {
        return p
            .strip_prefix(workspace_root)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|_| dir.to_string());
    }
    if let Some(base) = directory {
        let abs = Path::new(base).join(dir);
        return abs
            .strip_prefix(workspace_root)
            .map(normalize_path)
            .unwrap_or_else(|_| normalize_path(&abs));
    }
    dir.to_string()
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
    fn parse_compile_commands_extracts_per_file_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        let cc = ws.join("compile_commands.json");
        std::fs::write(
            &cc,
            serde_json::json!([
                {
                    "directory": ws.to_str().unwrap(),
                    "file": "foo.c",
                    "command": "gcc -I/usr/include -isystem /opt/include -Ivendor/inc -c foo.c"
                },
                {
                    "directory": ws.to_str().unwrap(),
                    "file": "bar.c",
                    "arguments": ["gcc", "-I", "lib/inc", "-isystem", "sys/inc", "-c", "bar.c"]
                }
            ])
            .to_string(),
        )
        .unwrap();
        let cc = parse_compile_commands(ws);

        let foo_dirs = cc.dirs_for("foo.c");
        assert!(foo_dirs.contains(&"/usr/include".to_string()));
        assert!(foo_dirs.contains(&"/opt/include".to_string()));
        assert!(foo_dirs.contains(&"vendor/inc".to_string()));
        assert!(!foo_dirs.contains(&"lib/inc".to_string()));

        let bar_dirs = cc.dirs_for("bar.c");
        assert!(bar_dirs.contains(&"lib/inc".to_string()));
        assert!(bar_dirs.contains(&"sys/inc".to_string()));
        assert!(!bar_dirs.contains(&"vendor/inc".to_string()));

        // Fallback contains all
        let all = cc.fallback_dirs();
        assert!(all.contains(&"/usr/include".to_string()));
        assert!(all.contains(&"lib/inc".to_string()));
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
        let cc = parse_compile_commands(dir.path());
        assert!(cc.per_file.is_empty());
        assert!(cc.fallback_dirs().is_empty());
    }

    #[test]
    fn per_file_scoping_resolves_correct_header() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();

        std::fs::write(
            ws.join("compile_commands.json"),
            serde_json::json!([
                {
                    "directory": ws.to_str().unwrap(),
                    "file": "src/a.c",
                    "command": "gcc -Iinc_a -c src/a.c"
                },
                {
                    "directory": ws.to_str().unwrap(),
                    "file": "src/b.c",
                    "command": "gcc -Iinc_b -c src/b.c"
                }
            ])
            .to_string(),
        )
        .unwrap();

        let cc = parse_compile_commands(ws);

        let mut id_to_path = HashMap::new();
        id_to_path.insert(1_i64, "src/a.c");
        id_to_path.insert(2_i64, "src/b.c");

        let mut path_to_id = HashMap::new();
        path_to_id.insert("inc_a/config.h", 10_i64);
        path_to_id.insert("inc_b/config.h", 20_i64);

        // a.c includes "config.h" → should find inc_a/config.h (id 10)
        let dirs_a = cc.dirs_for("src/a.c");
        let result_a = resolve_quoted_include("config.h", 1, &id_to_path, &path_to_id, ws, dirs_a);
        assert_eq!(result_a, Some(10));

        // b.c includes "config.h" → should find inc_b/config.h (id 20)
        let dirs_b = cc.dirs_for("src/b.c");
        let result_b = resolve_quoted_include("config.h", 2, &id_to_path, &path_to_id, ws, dirs_b);
        assert_eq!(result_b, Some(20));
    }

    #[test]
    fn relative_include_dir_resolved_against_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();

        std::fs::write(
            ws.join("compile_commands.json"),
            serde_json::json!([
                {
                    "directory": ws.join("build").to_str().unwrap(),
                    "file": "../src/main.c",
                    "command": "gcc -I../vendor/inc -c ../src/main.c"
                }
            ])
            .to_string(),
        )
        .unwrap();

        let cc = parse_compile_commands(ws);
        let dirs = cc.dirs_for("src/main.c");
        assert_eq!(dirs, &["vendor/inc".to_string()]);
    }

    #[test]
    fn fallback_when_no_entry_matches() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();

        std::fs::write(
            ws.join("compile_commands.json"),
            serde_json::json!([
                {
                    "directory": ws.to_str().unwrap(),
                    "file": "src/known.c",
                    "command": "gcc -Iinc -c src/known.c"
                }
            ])
            .to_string(),
        )
        .unwrap();

        let cc = parse_compile_commands(ws);

        // Unknown file gets empty per-file dirs
        assert!(cc.dirs_for("src/unknown.c").is_empty());

        // But fallback still has the dirs
        assert_eq!(cc.fallback_dirs(), &["inc".to_string()]);

        // Verify resolution works via fallback
        let mut id_to_path = HashMap::new();
        id_to_path.insert(1_i64, "src/unknown.c");
        let mut path_to_id = HashMap::new();
        path_to_id.insert("inc/header.h", 2_i64);

        let result = resolve_quoted_include(
            "header.h",
            1,
            &id_to_path,
            &path_to_id,
            ws,
            cc.fallback_dirs(),
        );
        assert_eq!(result, Some(2));
    }
}
