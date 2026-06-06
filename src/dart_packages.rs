use std::collections::HashMap;
use std::path::{Component, Path};

use tracing::info;

use crate::db::Db;
use crate::error::Result;

struct DartPackageMap {
    packages: HashMap<String, String>,
}

const SKIP_DIRS: &[&str] = &[
    "build",
    ".dart_tool",
    ".pub-cache",
    ".pub",
    "target",
    "node_modules",
];

impl DartPackageMap {
    fn build(workspace_root: &Path) -> Self {
        let mut packages = HashMap::new();
        let mut stack = vec![workspace_root.to_path_buf()];

        while let Some(dir) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name_str = name.to_string_lossy();

                if path.is_dir() {
                    if name_str.starts_with('.') || SKIP_DIRS.contains(&name_str.as_ref()) {
                        continue;
                    }
                    stack.push(path);
                } else if name_str == "pubspec.yaml" {
                    if let Some((pkg_name, lib_dir)) =
                        extract_package_info(workspace_root, &path)
                    {
                        packages.insert(pkg_name, lib_dir);
                    }
                }
            }
        }

        DartPackageMap { packages }
    }
}

fn extract_package_info(workspace_root: &Path, pubspec_path: &Path) -> Option<(String, String)> {
    let contents = std::fs::read_to_string(pubspec_path).ok()?;
    let pkg_name = contents
        .lines()
        .find(|line| line.starts_with("name:"))?
        .strip_prefix("name:")?
        .trim()
        .to_string();

    if pkg_name.is_empty() {
        return None;
    }

    let parent = pubspec_path.parent()?;
    let lib_dir = parent.join("lib");
    if !lib_dir.is_dir() {
        return None;
    }

    let rel = parent.strip_prefix(workspace_root).ok()?;
    let rel_str = rel.to_string_lossy();

    let lib_prefix = if rel_str.is_empty() {
        "lib/".to_string()
    } else {
        format!("{}/lib/", rel_str)
    };

    Some((pkg_name, lib_prefix))
}

fn resolve_package_uri(uri: &str, pkg_map: &DartPackageMap) -> Option<String> {
    let rest = uri.strip_prefix("package:")?;
    let (pkg_name, subpath) = rest.split_once('/')?;
    let lib_dir = pkg_map.packages.get(pkg_name)?;
    Some(format!("{}{}", lib_dir, subpath))
}

fn resolve_relative_import(
    path: &str,
    file_id: i64,
    id_to_path: &HashMap<i64, &str>,
) -> Option<String> {
    let importing_path = id_to_path.get(&file_id)?;
    let parent = Path::new(importing_path).parent()?;
    let joined = parent.join(path);
    normalize_path(&joined)
}

fn normalize_path(path: &Path) -> Option<String> {
    let mut components = Vec::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                if components.is_empty() {
                    return None;
                }
                components.pop();
            }
            Component::CurDir => {}
            Component::Normal(s) => {
                components.push(s.to_string_lossy().into_owned());
            }
            _ => return None,
        }
    }
    Some(components.join("/"))
}

pub fn resolve_dart_imports(db: &Db, workspace_root: &Path) -> Result<usize> {
    let unresolved = db.unresolved_dart_imports()?;
    if unresolved.is_empty() {
        return Ok(0);
    }

    let all_files = db.all_files()?;
    let path_to_id: HashMap<&str, i64> =
        all_files.iter().map(|f| (f.path.as_str(), f.id)).collect();
    let id_to_path: HashMap<i64, &str> =
        all_files.iter().map(|f| (f.id, f.path.as_str())).collect();

    let pkg_map = DartPackageMap::build(workspace_root);
    if !pkg_map.packages.is_empty() {
        info!(
            packages = pkg_map.packages.len(),
            "built Dart package map"
        );
    }

    let mut resolved_count = 0;
    for (import_id, file_id, imported_path) in &unresolved {
        let resolved_path = if imported_path.starts_with("package:") {
            resolve_package_uri(imported_path, &pkg_map)
        } else if imported_path.ends_with(".dart") && !imported_path.starts_with("dart:") {
            resolve_relative_import(imported_path, *file_id, &id_to_path)
        } else {
            None
        };

        if let Some(path) = resolved_path {
            if let Some(&target_file_id) = path_to_id.get(path.as_str()) {
                db.update_import_resolved_file_id(*import_id, target_file_id)?;
                resolved_count += 1;
            }
        }
    }

    Ok(resolved_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_package_uri() {
        let mut packages = HashMap::new();
        packages.insert("arrow_options".to_string(), "options/lib/".to_string());
        packages.insert("arrow_core".to_string(), "core/lib/".to_string());
        let pkg_map = DartPackageMap { packages };

        assert_eq!(
            resolve_package_uri("package:arrow_options/arrow_options.dart", &pkg_map),
            Some("options/lib/arrow_options.dart".to_string())
        );
        assert_eq!(
            resolve_package_uri("package:arrow_core/src/models/chart.dart", &pkg_map),
            Some("core/lib/src/models/chart.dart".to_string())
        );
    }

    #[test]
    fn test_resolve_package_uri_unknown_package() {
        let pkg_map = DartPackageMap {
            packages: HashMap::new(),
        };
        assert_eq!(
            resolve_package_uri("package:unknown/foo.dart", &pkg_map),
            None
        );
    }

    #[test]
    fn test_resolve_package_uri_not_package_scheme() {
        let pkg_map = DartPackageMap {
            packages: HashMap::new(),
        };
        assert_eq!(resolve_package_uri("dart:core", &pkg_map), None);
        assert_eq!(resolve_package_uri("../foo.dart", &pkg_map), None);
    }

    #[test]
    fn test_resolve_relative_import() {
        let mut id_to_path = HashMap::new();
        id_to_path.insert(1_i64, "options/lib/src/widget.dart");

        assert_eq!(
            resolve_relative_import("utils.dart", 1, &id_to_path),
            Some("options/lib/src/utils.dart".to_string())
        );
        assert_eq!(
            resolve_relative_import("../models/foo.dart", 1, &id_to_path),
            Some("options/lib/models/foo.dart".to_string())
        );
        assert_eq!(
            resolve_relative_import("./bar.dart", 1, &id_to_path),
            Some("options/lib/src/bar.dart".to_string())
        );
    }

    #[test]
    fn test_resolve_relative_import_escapes_root() {
        let mut id_to_path = HashMap::new();
        id_to_path.insert(1_i64, "lib/foo.dart");

        assert_eq!(
            resolve_relative_import("../../escape.dart", 1, &id_to_path),
            None
        );
    }

    #[test]
    fn test_normalize_path() {
        assert_eq!(
            normalize_path(Path::new("a/b/../c/d")),
            Some("a/c/d".to_string())
        );
        assert_eq!(
            normalize_path(Path::new("a/./b/c")),
            Some("a/b/c".to_string())
        );
        assert_eq!(
            normalize_path(Path::new("a/b/c")),
            Some("a/b/c".to_string())
        );
        assert_eq!(normalize_path(Path::new("../escape")), None);
    }

    #[test]
    fn test_extract_package_info() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path().join("my_pkg");
        std::fs::create_dir_all(pkg_dir.join("lib")).unwrap();
        std::fs::write(
            pkg_dir.join("pubspec.yaml"),
            "name: my_package\nversion: 1.0.0\n",
        )
        .unwrap();

        let result = extract_package_info(dir.path(), &pkg_dir.join("pubspec.yaml"));
        assert_eq!(
            result,
            Some(("my_package".to_string(), "my_pkg/lib/".to_string()))
        );
    }

    #[test]
    fn test_extract_package_info_root_pubspec() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        std::fs::write(
            dir.path().join("pubspec.yaml"),
            "name: root_pkg\nversion: 1.0.0\n",
        )
        .unwrap();

        let result = extract_package_info(dir.path(), &dir.path().join("pubspec.yaml"));
        assert_eq!(
            result,
            Some(("root_pkg".to_string(), "lib/".to_string()))
        );
    }

    #[test]
    fn test_extract_package_info_no_lib_dir() {
        let dir = tempfile::tempdir().unwrap();
        let pkg_dir = dir.path().join("broken_pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(
            pkg_dir.join("pubspec.yaml"),
            "name: broken\nversion: 1.0.0\n",
        )
        .unwrap();

        let result = extract_package_info(dir.path(), &pkg_dir.join("pubspec.yaml"));
        assert_eq!(result, None);
    }

    #[test]
    fn test_build_package_map() {
        let dir = tempfile::tempdir().unwrap();

        for (name, pkg) in [("options", "arrow_options"), ("core", "arrow_core")] {
            let pkg_dir = dir.path().join(name);
            std::fs::create_dir_all(pkg_dir.join("lib")).unwrap();
            std::fs::write(
                pkg_dir.join("pubspec.yaml"),
                format!("name: {pkg}\nversion: 1.0.0\n"),
            )
            .unwrap();
        }

        // Should be skipped — inside .dart_tool
        let hidden = dir.path().join(".dart_tool").join("nested");
        std::fs::create_dir_all(hidden.join("lib")).unwrap();
        std::fs::write(
            hidden.join("pubspec.yaml"),
            "name: should_skip\nversion: 1.0.0\n",
        )
        .unwrap();

        let pkg_map = DartPackageMap::build(dir.path());
        assert_eq!(pkg_map.packages.len(), 2);
        assert_eq!(
            pkg_map.packages.get("arrow_options"),
            Some(&"options/lib/".to_string())
        );
        assert_eq!(
            pkg_map.packages.get("arrow_core"),
            Some(&"core/lib/".to_string())
        );
        assert!(!pkg_map.packages.contains_key("should_skip"));
    }

    #[test]
    fn test_dart_sdk_imports_skipped() {
        let pkg_map = DartPackageMap {
            packages: HashMap::new(),
        };
        assert_eq!(resolve_package_uri("dart:core", &pkg_map), None);
        assert_eq!(resolve_package_uri("dart:async", &pkg_map), None);
    }
}
