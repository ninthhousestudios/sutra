use std::collections::{HashMap, HashSet};
use std::path::Path;

use tracing::debug;

use crate::db::Db;
use crate::error::Result;

pub fn resolve_python_imports(db: &Db, workspace_root: &Path) -> Result<usize> {
    let unresolved = db.unresolved_python_imports()?;
    if unresolved.is_empty() {
        return Ok(0);
    }

    let all_files = db.all_files()?;
    let path_to_id: HashMap<&str, i64> = all_files.iter().map(|f| (&*f.path, f.id)).collect();
    let id_to_path: HashMap<i64, &str> = all_files.iter().map(|f| (f.id, &*f.path)).collect();

    let package_roots = discover_package_roots(&all_files, workspace_root);

    let mut updates = Vec::new();
    for (import_id, file_id, raw_path, kind) in &unresolved {
        let resolved = if raw_path.starts_with('.') {
            resolve_relative(raw_path, *file_id, &id_to_path, &path_to_id)
        } else {
            resolve_absolute(raw_path, &package_roots, &path_to_id, kind)
        };

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
            "resolved Python import edges"
        );
    }
    Ok(resolved_count)
}

fn discover_package_roots(files: &[crate::db::FileRow], workspace_root: &Path) -> Vec<String> {
    if let Ok(rules) = crate::rules::load_rules(workspace_root)
        && let Some(ref py) = rules.python
        && let Some(ref roots) = py.package_roots
    {
        return roots.clone();
    }

    let mut package_dirs: HashSet<&str> = HashSet::new();
    for f in files {
        if f.language == "python"
            && f.path.ends_with("__init__.py")
            && let Some((dir, _)) = f.path.rsplit_once('/')
        {
            package_dirs.insert(dir);
        }
    }

    if package_dirs.is_empty() {
        return Vec::new();
    }

    let mut roots: HashSet<&str> = HashSet::new();
    for &dir in &package_dirs {
        let mut topmost = dir;
        let mut current = dir;
        while let Some((parent, _)) = current.rsplit_once('/') {
            if package_dirs.contains(parent) {
                topmost = parent;
            }
            current = parent;
        }
        match topmost.rsplit_once('/') {
            Some((parent, _)) => {
                roots.insert(parent);
            }
            None => {
                roots.insert("");
            }
        }
    }

    let mut result: Vec<String> = roots.into_iter().map(|s| s.to_string()).collect();
    result.sort();
    result
}

fn resolve_absolute(
    raw_path: &str,
    package_roots: &[String],
    path_to_id: &HashMap<&str, i64>,
    kind: &str,
) -> Option<i64> {
    let segments: Vec<&str> = raw_path.split('.').collect();
    let strict = kind == "import";
    for root in package_roots {
        let result = if strict {
            try_resolve_strict(root, &segments, path_to_id)
        } else {
            try_resolve_segments(root, &segments, path_to_id)
        };
        if let Some(id) = result {
            return Some(id);
        }
    }
    None
}

/// Strict resolution: only try the full segment path, no prefix fallback.
/// Used for plain `import` statements where the full dotted path must be a module.
fn try_resolve_strict(
    base: &str,
    segments: &[&str],
    path_to_id: &HashMap<&str, i64>,
) -> Option<i64> {
    let file_part = segments.join("/");
    let dir_part = if base.is_empty() {
        file_part
    } else {
        format!("{base}/{file_part}")
    };

    let py_path = format!("{dir_part}.py");
    if let Some(&id) = path_to_id.get(py_path.as_str()) {
        return Some(id);
    }

    let init_path = format!("{dir_part}/__init__.py");
    path_to_id.get(init_path.as_str()).copied()
}

fn resolve_relative(
    raw_path: &str,
    file_id: i64,
    id_to_path: &HashMap<i64, &str>,
    path_to_id: &HashMap<&str, i64>,
) -> Option<i64> {
    let file_path = *id_to_path.get(&file_id)?;

    let dots = raw_path.bytes().take_while(|&b| b == b'.').count();
    let remaining = &raw_path[dots..];

    let dir = match file_path.rsplit_once('/') {
        Some((d, _)) => d,
        None => "",
    };
    let mut parts: Vec<&str> = if dir.is_empty() {
        Vec::new()
    } else {
        dir.split('/').collect()
    };

    for _ in 1..dots {
        parts.pop()?;
    }

    let base = parts.join("/");

    if remaining.is_empty() {
        let init_path = if base.is_empty() {
            "__init__.py".to_string()
        } else {
            format!("{base}/__init__.py")
        };
        return path_to_id.get(init_path.as_str()).copied();
    }

    let segments: Vec<&str> = remaining.split('.').collect();
    try_resolve_segments(&base, &segments, path_to_id)
}

fn try_resolve_segments(
    base: &str,
    segments: &[&str],
    path_to_id: &HashMap<&str, i64>,
) -> Option<i64> {
    for prefix_len in (1..=segments.len()).rev() {
        let file_part = segments[..prefix_len].join("/");
        let dir_part = if base.is_empty() {
            file_part
        } else {
            format!("{base}/{file_part}")
        };

        let py_path = format!("{dir_part}.py");
        if let Some(&id) = path_to_id.get(py_path.as_str()) {
            return Some(id);
        }

        let init_path = format!("{dir_part}/__init__.py");
        if let Some(&id) = path_to_id.get(init_path.as_str()) {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_path_map<'a>(paths: &'a [&'a str]) -> (HashMap<&'a str, i64>, HashMap<i64, &'a str>) {
        let mut path_to_id = HashMap::new();
        let mut id_to_path = HashMap::new();
        for (i, &path) in paths.iter().enumerate() {
            let id = (i + 1) as i64;
            path_to_id.insert(path, id);
            id_to_path.insert(id, path);
        }
        (path_to_id, id_to_path)
    }

    #[test]
    fn absolute_foo_bar_resolves_to_py_file() {
        let (path_to_id, _) = make_path_map(&["foo/__init__.py", "foo/bar.py"]);
        let roots = vec![String::new()];
        assert_eq!(
            resolve_absolute("foo.bar", &roots, &path_to_id, "from_import"),
            Some(2)
        );
    }

    #[test]
    fn package_resolves_to_init() {
        let (path_to_id, _) = make_path_map(&["foo/__init__.py"]);
        let roots = vec![String::new()];
        assert_eq!(
            resolve_absolute("foo", &roots, &path_to_id, "import"),
            Some(1)
        );
    }

    #[test]
    fn relative_single_dot_import() {
        let (path_to_id, id_to_path) =
            make_path_map(&["pkg/__init__.py", "pkg/main.py", "pkg/sibling.py"]);
        assert_eq!(
            resolve_relative(".sibling", 2, &id_to_path, &path_to_id),
            Some(3)
        );
    }

    #[test]
    fn relative_double_dot_import() {
        let (path_to_id, id_to_path) = make_path_map(&[
            "pkg/__init__.py",
            "pkg/utils.py",
            "pkg/sub/__init__.py",
            "pkg/sub/mod.py",
        ]);
        assert_eq!(
            resolve_relative("..utils.helper", 4, &id_to_path, &path_to_id),
            Some(2)
        );
    }

    #[test]
    fn unresolved_external_returns_none() {
        let (path_to_id, _) = make_path_map(&["foo/__init__.py", "foo/bar.py"]);
        let roots = vec![String::new()];
        assert_eq!(
            resolve_absolute("pathlib", &roots, &path_to_id, "import"),
            None
        );
        assert_eq!(
            resolve_absolute("os.path", &roots, &path_to_id, "import"),
            None
        );
    }

    #[test]
    fn src_layout_with_configured_roots() {
        let (path_to_id, _) =
            make_path_map(&["src/mypackage/__init__.py", "src/mypackage/module.py"]);
        let roots = vec!["src".to_string()];
        assert_eq!(
            resolve_absolute("mypackage.module", &roots, &path_to_id, "from_import"),
            Some(2)
        );
    }

    #[test]
    fn discover_roots_flat_layout() {
        let files = vec![
            make_file_row(1, "mypackage/__init__.py"),
            make_file_row(2, "mypackage/sub/__init__.py"),
        ];
        let roots = discover_package_roots(&files, Path::new("/nonexistent"));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], "");
    }

    #[test]
    fn discover_roots_src_layout() {
        let files = vec![make_file_row(1, "src/mypackage/__init__.py")];
        let roots = discover_package_roots(&files, Path::new("/nonexistent"));
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], "src");
    }

    #[test]
    fn discover_roots_empty_without_init_files() {
        let files = vec![make_file_row(1, "scripts/run.py")];
        let roots = discover_package_roots(&files, Path::new("/nonexistent"));
        assert!(roots.is_empty());
    }

    #[test]
    fn relative_wildcard_resolves_to_init() {
        let (path_to_id, id_to_path) = make_path_map(&["pkg/__init__.py", "pkg/main.py"]);
        assert_eq!(resolve_relative(".", 2, &id_to_path, &path_to_id), Some(1));
    }

    #[test]
    fn plain_import_strict_no_prefix_fallback() {
        // `import pkg.foo.bar` with only pkg/foo.py present should NOT resolve
        let (path_to_id, _) = make_path_map(&["pkg/__init__.py", "pkg/foo.py"]);
        let roots = vec![String::new()];
        assert_eq!(
            resolve_absolute("pkg.foo.bar", &roots, &path_to_id, "import"),
            None
        );
    }

    #[test]
    fn from_import_allows_prefix_fallback() {
        // `from pkg.foo import bar` with only pkg/foo.py present SHOULD resolve
        let (path_to_id, _) = make_path_map(&["pkg/__init__.py", "pkg/foo.py"]);
        let roots = vec![String::new()];
        assert_eq!(
            resolve_absolute("pkg.foo.bar", &roots, &path_to_id, "from_import"),
            Some(2)
        );
    }

    fn make_file_row(id: i64, path: &str) -> crate::db::FileRow {
        use std::sync::Arc;
        crate::db::FileRow {
            id,
            path: Arc::from(path),
            language: "python".to_string(),
            content_hash: String::new(),
            line_count: 0,
            parsed_ok: true,
            last_parsed: String::new(),
            fan_in_files: 0,
            blast_radius: 0,
            pagerank: None,
        }
    }
}
