use std::collections::HashMap;
use std::path::Path;

use tracing::debug;

use crate::db::Db;
use crate::error::Result;

// ── Workspace layout ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct WorkspaceMember {
    pub name: String,
    pub dir: String,
}

#[derive(Debug, Clone)]
pub struct WorkspaceLayout {
    pub root_crate: Option<String>,
    pub members: Vec<WorkspaceMember>,
}

impl WorkspaceLayout {
    fn crate_for_file(&self, file_path: &str) -> Option<(&str, String)> {
        for m in &self.members {
            let prefix = format!("{}/", m.dir);
            if file_path.starts_with(&prefix) {
                return Some((&m.name, format!("{}/src", m.dir)));
            }
        }
        self.root_crate
            .as_deref()
            .map(|name| (name, "src".to_string()))
    }

    fn src_prefix_for_crate(&self, crate_name: &str) -> Option<String> {
        if self.root_crate.as_deref() == Some(crate_name) {
            return Some("src".to_string());
        }
        self.members
            .iter()
            .find(|m| m.name == crate_name)
            .map(|m| format!("{}/src", m.dir))
    }

    pub fn all_crate_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.members.iter().map(|m| m.name.as_str()).collect();
        if let Some(ref root) = self.root_crate {
            names.push(root.as_str());
        }
        names
    }
}

pub fn parse_workspace_layout(workspace_root: &Path) -> WorkspaceLayout {
    let empty = WorkspaceLayout {
        root_crate: None,
        members: Vec::new(),
    };
    let cargo_path = workspace_root.join("Cargo.toml");
    let content = match std::fs::read_to_string(&cargo_path) {
        Ok(c) => c,
        Err(_) => return empty,
    };
    let parsed: toml::Value = match content.parse() {
        Ok(v) => v,
        Err(_) => return empty,
    };

    let root_crate = lib_crate_name(&parsed);

    let member_globs: Vec<String> = parsed
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let mut members = Vec::new();
    for pattern in &member_globs {
        let abs_pattern = workspace_root.join(pattern);
        let Ok(paths) = glob::glob(&abs_pattern.to_string_lossy()) else {
            continue;
        };
        for entry in paths.flatten() {
            if let Some(m) = read_workspace_member(workspace_root, &entry) {
                members.push(m);
            }
        }
    }

    WorkspaceLayout {
        root_crate,
        members,
    }
}

fn read_workspace_member(workspace_root: &Path, member_dir: &Path) -> Option<WorkspaceMember> {
    if !member_dir.is_dir() {
        return None;
    }
    let cargo = std::fs::read_to_string(member_dir.join("Cargo.toml")).ok()?;
    let parsed: toml::Value = cargo.parse().ok()?;
    let name = lib_crate_name(&parsed)?;
    let dir = member_dir
        .strip_prefix(workspace_root)
        .ok()?
        .to_string_lossy()
        .to_string();
    Some(WorkspaceMember { name, dir })
}

/// Cargo import crate name: [lib].name if set, else [package].name with hyphens → underscores.
fn lib_crate_name(parsed: &toml::Value) -> Option<String> {
    if let Some(lib_name) = parsed
        .get("lib")
        .and_then(|l| l.get("name"))
        .and_then(|n| n.as_str())
    {
        return Some(lib_name.to_string());
    }
    parsed
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.replace('-', "_"))
}

// ── Import resolution ─────────────────────────────────────────────────

pub fn resolve_rust_imports(db: &Db, workspace_root: &Path) -> Result<usize> {
    let layout = parse_workspace_layout(workspace_root);

    let unresolved = db.unresolved_rust_imports()?;
    if unresolved.is_empty() {
        return Ok(0);
    }

    let all_files = db.all_files()?;
    let path_to_id: HashMap<&str, i64> = all_files.iter().map(|f| (&*f.path, f.id)).collect();
    let id_to_path: HashMap<i64, &str> = all_files.iter().map(|f| (f.id, &*f.path)).collect();

    let mut updates = Vec::new();
    for (import_id, file_id, imported_path) in &unresolved {
        let file_path = match id_to_path.get(file_id) {
            Some(p) => *p,
            None => continue,
        };
        let resolved = match normalize_to_crate_segments(imported_path, file_path, &layout) {
            Some(r) if !r.segments.is_empty() => r,
            _ => continue,
        };
        if let Some(target_id) =
            resolve_segments(&resolved.segments, &path_to_id, &resolved.src_prefix)
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
            "resolved Rust import edges"
        );
    }
    Ok(resolved_count)
}

pub fn read_crate_name(workspace_root: &Path) -> Option<String> {
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

// ── Cross-crate resolution ────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedImport {
    pub segments: Vec<String>,
    pub src_prefix: String,
}

/// Convert an import path into module segments relative to a crate root.
/// Returns `None` for external crate imports.
pub fn normalize_to_crate_segments(
    imported_path: &str,
    importing_file: &str,
    layout: &WorkspaceLayout,
) -> Option<ResolvedImport> {
    let path = imported_path.strip_suffix("::*").unwrap_or(imported_path);

    let (own_crate, own_src) = layout.crate_for_file(importing_file)?;

    // crate:: prefix — resolve within the importing file's own crate
    if let Some(rest) = path.strip_prefix("crate::") {
        return Some(ResolvedImport {
            segments: rest.split("::").map(String::from).collect(),
            src_prefix: own_src,
        });
    }

    // Own crate name prefix (e.g. `use vidya::format` from vidya's src/)
    let own_prefix = format!("{own_crate}::");
    if let Some(rest) = path.strip_prefix(&own_prefix) {
        return Some(ResolvedImport {
            segments: rest.split("::").map(String::from).collect(),
            src_prefix: own_src.clone(),
        });
    }
    if path == own_crate {
        return Some(ResolvedImport {
            segments: vec![],
            src_prefix: own_src.clone(),
        });
    }

    // Sibling/workspace crate name prefix (e.g. `use vidya_core::query`)
    let first_segment = path.split("::").next().unwrap_or("");
    if first_segment != own_crate {
        if let Some(target_src) = layout.src_prefix_for_crate(first_segment) {
            let rest = path.strip_prefix(first_segment).unwrap_or("");
            let rest = rest.strip_prefix("::").unwrap_or("");
            return Some(ResolvedImport {
                segments: if rest.is_empty() {
                    vec![]
                } else {
                    rest.split("::").map(String::from).collect()
                },
                src_prefix: target_src,
            });
        }
    }

    if path == "super" || path.starts_with("super::") {
        let rest = path.strip_prefix("super::").unwrap_or("");
        let mut parent = file_to_module_segments(importing_file, &own_src);
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
        return Some(ResolvedImport {
            segments: parent,
            src_prefix: own_src,
        });
    }

    if path == "self" || path.starts_with("self::") {
        let rest = path.strip_prefix("self::").unwrap_or("");
        let mut segs = file_to_module_segments(importing_file, &own_src);
        if !rest.is_empty() {
            segs.extend(rest.split("::").map(String::from));
        }
        return Some(ResolvedImport {
            segments: segs,
            src_prefix: own_src,
        });
    }

    None
}

fn file_to_module_segments(file_path: &str, src_prefix: &str) -> Vec<String> {
    let prefix = format!("{src_prefix}/");
    let stripped = file_path.strip_prefix(&prefix).unwrap_or(file_path);

    let without_ext = stripped.strip_suffix(".rs").unwrap_or(stripped);

    let parts: Vec<&str> = without_ext.split('/').collect();
    parts
        .into_iter()
        .filter(|p| *p != "lib" && *p != "mod" && *p != "main")
        .map(String::from)
        .collect()
}

pub fn resolve_segments(
    segments: &[String],
    path_to_id: &HashMap<&str, i64>,
    src_prefix: &str,
) -> Option<i64> {
    for depth in (1..=segments.len()).rev() {
        let joined = segments[..depth]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("/");

        let file_path = format!("{src_prefix}/{joined}.rs");
        if let Some(&id) = path_to_id.get(file_path.as_str()) {
            return Some(id);
        }

        let mod_path = format!("{src_prefix}/{joined}/mod.rs");
        if let Some(&id) = path_to_id.get(mod_path.as_str()) {
            return Some(id);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single_crate(name: &str) -> WorkspaceLayout {
        WorkspaceLayout {
            root_crate: Some(name.to_string()),
            members: Vec::new(),
        }
    }

    fn workspace_layout() -> WorkspaceLayout {
        WorkspaceLayout {
            root_crate: Some("vidya".to_string()),
            members: vec![WorkspaceMember {
                name: "vidya_core".to_string(),
                dir: "vidya-core".to_string(),
            }],
        }
    }

    // ── normalize_to_crate_segments (single-crate, backward compat) ───

    #[test]
    fn crate_prefix() {
        let layout = single_crate("sutra");
        let r = normalize_to_crate_segments("crate::db::Db", "src/tools/orient.rs", &layout);
        let r = r.unwrap();
        assert_eq!(r.segments, vec!["db", "Db"]);
        assert_eq!(r.src_prefix, "src");
    }

    #[test]
    fn crate_name_prefix() {
        let layout = single_crate("sutra");
        let r = normalize_to_crate_segments("sutra::db::Db", "src/tools/orient.rs", &layout);
        let r = r.unwrap();
        assert_eq!(r.segments, vec!["db", "Db"]);
        assert_eq!(r.src_prefix, "src");
    }

    #[test]
    fn super_prefix() {
        let layout = single_crate("sutra");
        let r =
            normalize_to_crate_segments("super::scoring::Signal", "src/tools/review.rs", &layout);
        assert_eq!(r.unwrap().segments, vec!["tools", "scoring", "Signal"]);
    }

    #[test]
    fn super_from_mod_rs() {
        let layout = single_crate("sutra");
        let r = normalize_to_crate_segments("super::workspace", "src/tools/mod.rs", &layout);
        assert_eq!(r.unwrap().segments, vec!["workspace"]);
    }

    #[test]
    fn super_glob() {
        let layout = single_crate("sutra");
        let r = normalize_to_crate_segments("super::*", "src/db/conventions.rs", &layout);
        assert_eq!(r.unwrap().segments, vec!["db"]);
    }

    #[test]
    fn self_prefix() {
        let layout = single_crate("sutra");
        let r = normalize_to_crate_segments("self::engine", "src/constraints/mod.rs", &layout);
        assert_eq!(r.unwrap().segments, vec!["constraints", "engine"]);
    }

    #[test]
    fn external_crate_returns_none() {
        let layout = single_crate("sutra");
        let r = normalize_to_crate_segments(
            "std::collections::HashMap",
            "src/tools/orient.rs",
            &layout,
        );
        assert!(r.is_none());
    }

    // ── normalize_to_crate_segments (multi-crate workspace) ───────────

    #[test]
    fn cross_crate_import() {
        let layout = workspace_layout();
        let r =
            normalize_to_crate_segments("vidya_core::query::QueryEngine", "src/main.rs", &layout);
        let r = r.unwrap();
        assert_eq!(r.segments, vec!["query", "QueryEngine"]);
        assert_eq!(r.src_prefix, "vidya-core/src");
    }

    #[test]
    fn cross_crate_bare_name() {
        let layout = workspace_layout();
        let r = normalize_to_crate_segments("vidya_core", "src/main.rs", &layout);
        let r = r.unwrap();
        assert!(r.segments.is_empty());
        assert_eq!(r.src_prefix, "vidya-core/src");
    }

    #[test]
    fn cross_crate_glob() {
        let layout = workspace_layout();
        let r = normalize_to_crate_segments("vidya_core::resolve::*", "src/main.rs", &layout);
        let r = r.unwrap();
        assert_eq!(r.segments, vec!["resolve"]);
        assert_eq!(r.src_prefix, "vidya-core/src");
    }

    #[test]
    fn own_crate_from_member() {
        let layout = workspace_layout();
        let r = normalize_to_crate_segments(
            "crate::resolve::ResolvedToken",
            "vidya-core/src/lib.rs",
            &layout,
        );
        let r = r.unwrap();
        assert_eq!(r.segments, vec!["resolve", "ResolvedToken"]);
        assert_eq!(r.src_prefix, "vidya-core/src");
    }

    #[test]
    fn super_in_member_crate() {
        let layout = workspace_layout();
        let r = normalize_to_crate_segments(
            "super::ontology",
            "vidya-core/src/resolve/mod.rs",
            &layout,
        );
        let r = r.unwrap();
        assert_eq!(r.segments, vec!["ontology"]);
        assert_eq!(r.src_prefix, "vidya-core/src");
    }

    #[test]
    fn self_in_member_crate() {
        let layout = workspace_layout();
        let r = normalize_to_crate_segments(
            "self::ResolvedToken",
            "vidya-core/src/resolve/mod.rs",
            &layout,
        );
        let r = r.unwrap();
        assert_eq!(r.segments, vec!["resolve", "ResolvedToken"]);
        assert_eq!(r.src_prefix, "vidya-core/src");
    }

    #[test]
    fn external_from_workspace_still_none() {
        let layout = workspace_layout();
        let r = normalize_to_crate_segments("serde::Deserialize", "vidya-core/src/lib.rs", &layout);
        assert!(r.is_none());
    }

    // ── file_to_module_segments ───────────────────────────────────────

    #[test]
    fn file_to_module_basic() {
        assert_eq!(
            file_to_module_segments("src/db/conventions.rs", "src"),
            vec!["db", "conventions"]
        );
        assert_eq!(file_to_module_segments("src/db/mod.rs", "src"), vec!["db"]);
        assert_eq!(
            file_to_module_segments("src/error.rs", "src"),
            vec!["error"]
        );
        let empty: Vec<String> = vec![];
        assert_eq!(file_to_module_segments("src/lib.rs", "src"), empty);
    }

    #[test]
    fn file_to_module_member_crate() {
        assert_eq!(
            file_to_module_segments("vidya-core/src/query.rs", "vidya-core/src"),
            vec!["query"]
        );
        assert_eq!(
            file_to_module_segments("vidya-core/src/resolve/mod.rs", "vidya-core/src"),
            vec!["resolve"]
        );
        let empty: Vec<String> = vec![];
        assert_eq!(
            file_to_module_segments("vidya-core/src/lib.rs", "vidya-core/src"),
            empty
        );
    }

    // ── resolve_segments ──────────────────────────────────────────────

    #[test]
    fn resolve_prefers_deepest_match() {
        let mut path_to_id = HashMap::new();
        path_to_id.insert("src/db.rs", 1);
        path_to_id.insert("src/db/conventions.rs", 2);

        let segs: Vec<String> = vec!["db", "conventions", "ConventionRow"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(resolve_segments(&segs, &path_to_id, "src"), Some(2));
    }

    #[test]
    fn resolve_falls_back_to_mod_rs() {
        let mut path_to_id = HashMap::new();
        path_to_id.insert("src/conventions/mod.rs", 3);

        let segs: Vec<String> = vec!["conventions", "engine", "FcaEngine"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(resolve_segments(&segs, &path_to_id, "src"), Some(3));
    }

    // ── lib_crate_name ────────────────────────────────────────────────

    #[test]
    fn lib_name_preferred_over_package_name() {
        let toml: toml::Value = r#"
[package]
name = "vidya-core"

[lib]
name = "vidya_core_runtime"
"#
        .parse()
        .unwrap();
        assert_eq!(lib_crate_name(&toml).as_deref(), Some("vidya_core_runtime"));
    }

    #[test]
    fn package_name_fallback_when_no_lib() {
        let toml: toml::Value = r#"
[package]
name = "vidya-core"
"#
        .parse()
        .unwrap();
        assert_eq!(lib_crate_name(&toml).as_deref(), Some("vidya_core"));
    }

    // ── cross-crate with [lib].name ─────────────────────────────────

    #[test]
    fn cross_crate_uses_lib_name() {
        let layout = WorkspaceLayout {
            root_crate: Some("vidya".to_string()),
            members: vec![WorkspaceMember {
                name: "vidya_core_runtime".to_string(),
                dir: "vidya-core".to_string(),
            }],
        };
        let r = normalize_to_crate_segments(
            "vidya_core_runtime::query::Engine",
            "src/main.rs",
            &layout,
        );
        let r = r.unwrap();
        assert_eq!(r.segments, vec!["query", "Engine"]);
        assert_eq!(r.src_prefix, "vidya-core/src");
    }

    #[test]
    fn all_crate_names_includes_lib_name() {
        let layout = WorkspaceLayout {
            root_crate: Some("vidya".to_string()),
            members: vec![WorkspaceMember {
                name: "vidya_core_runtime".to_string(),
                dir: "vidya-core".to_string(),
            }],
        };
        let names = layout.all_crate_names();
        assert!(names.contains(&"vidya_core_runtime"));
        assert!(names.contains(&"vidya"));
    }

    // ── resolve_segments ──────────────────────────────────────────────

    #[test]
    fn resolve_in_member_crate() {
        let mut path_to_id = HashMap::new();
        path_to_id.insert("vidya-core/src/query.rs", 10);
        path_to_id.insert("vidya-core/src/resolve/mod.rs", 11);

        let segs: Vec<String> = vec!["query", "QueryEngine"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            resolve_segments(&segs, &path_to_id, "vidya-core/src"),
            Some(10)
        );

        let segs2: Vec<String> = vec!["resolve", "ResolvedToken"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            resolve_segments(&segs2, &path_to_id, "vidya-core/src"),
            Some(11)
        );
    }
}
