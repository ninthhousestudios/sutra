//! External-crate dependency constraints.
//!
//! `forbidden_external` and `confined_external` operate on imports of crates
//! that live OUTSIDE the workspace — exactly the rows the resolver leaves with
//! `resolved_file_id = NULL`. Two signals feed them:
//!
//! - use-statement / import-directive paths (`use axum::Router`,
//!   `import 'package:flutter/material.dart'`)
//! - Cargo manifest `[dependencies]` entries — the linking truth; a crate can
//!   link without a single use-statement.

use std::path::Path;

use glob::{MatchOptions, Pattern};

use crate::constraints::check::{ConstraintFinding, FindingDelta};
use crate::rules::{Constraint, ConstraintKind, Severity};

/// Extract the external crate/package name from a raw import path, or `None`
/// when the import is workspace-internal (or unrecognizable).
///
/// `workspace_crate_names` lists all crate names in the workspace (underscored),
/// used to exclude sibling crate imports from external classification.
pub fn external_crate_of_import(
    raw_path: &str,
    language: &str,
    workspace_crate_names: &[&str],
) -> Option<String> {
    match language {
        "rust" => {
            let path = raw_path.strip_suffix("::*").unwrap_or(raw_path);
            let first = path.split("::").next()?.trim();
            if first.is_empty() || matches!(first, "crate" | "self" | "super") {
                return None;
            }
            if workspace_crate_names.iter().any(|&c| c == first) {
                return None;
            }
            Some(first.to_string())
        }
        "dart" => {
            if let Some(rest) = raw_path.strip_prefix("package:") {
                let name = rest.split('/').next()?.trim();
                if name.is_empty() {
                    return None;
                }
                Some(name.to_string())
            } else if raw_path.starts_with("dart:") {
                Some(raw_path.to_string())
            } else {
                // relative import — workspace-internal
                None
            }
        }
        _ => None,
    }
}

/// Normalize a crate name for matching: Cargo package names use hyphens,
/// Rust import paths use underscores. Compare in underscore space.
fn normalize_crate(name: &str) -> String {
    name.replace('-', "_")
}

fn crate_matches(patterns: &[String], crate_name: &str) -> bool {
    let normalized = normalize_crate(crate_name);
    patterns.iter().any(|p| {
        Pattern::new(&normalize_crate(p))
            .ok()
            .is_some_and(|pat| pat.matches(&normalized))
    })
}

fn path_matches(pattern: &str, path: &str) -> bool {
    let opts = MatchOptions {
        require_literal_separator: true,
        ..MatchOptions::default()
    };
    Pattern::new(pattern)
        .ok()
        .is_some_and(|p| p.matches_with(path, opts))
}

/// Find the first external constraint violated by `from_path` importing
/// (or depending on) `crate_name`. `is_dev` marks dev/build-dependency
/// manifest entries, which only count against constraints with `include_dev`.
pub fn match_external<'a>(
    constraints: &'a [Constraint],
    from_path: &str,
    crate_name: &str,
    is_dev: bool,
) -> Option<&'a Constraint> {
    constraints.iter().find(|c| match &c.kind {
        ConstraintKind::ForbiddenExternal {
            from,
            crates,
            include_dev,
        } => {
            (!is_dev || *include_dev)
                && path_matches(from, from_path)
                && crate_matches(crates, crate_name)
        }
        ConstraintKind::ConfinedExternal {
            crates,
            allowed_in,
            include_dev,
        } => {
            (!is_dev || *include_dev)
                && crate_matches(crates, crate_name)
                && !allowed_in.iter().any(|a| path_matches(a, from_path))
        }
        _ => false,
    })
}

pub fn has_external_constraints(constraints: &[Constraint]) -> bool {
    constraints.iter().any(|c| {
        matches!(
            c.kind,
            ConstraintKind::ForbiddenExternal { .. } | ConstraintKind::ConfinedExternal { .. }
        )
    })
}

/// Error when a forbidden_external/confined_external constraint targets a
/// workspace member crate. Workspace members are resolved as internal edges;
/// use forbidden_dep instead.
pub fn validate_no_external_targeting_members(
    constraints: &[Constraint],
    workspace_crate_names: &[&str],
) -> Result<(), String> {
    if workspace_crate_names.is_empty() {
        return Ok(());
    }
    for c in constraints {
        let crates = match &c.kind {
            ConstraintKind::ForbiddenExternal { crates, .. }
            | ConstraintKind::ConfinedExternal { crates, .. } => crates,
            _ => continue,
        };
        for member in workspace_crate_names {
            if crate_matches(crates, member) {
                let name = c.name.as_deref().unwrap_or(&c.id);
                return Err(format!(
                    "constraint '{name}' targets workspace member '{member}' via {} \
                     — workspace members are internal; use forbidden_dep instead",
                    c.kind.kind_tag(),
                ));
            }
        }
    }
    Ok(())
}

pub fn config_error_finding(msg: &str) -> ConstraintFinding {
    ConstraintFinding {
        constraint_id: "config-error".to_string(),
        constraint_name: None,
        constraint_kind: "config_error".to_string(),
        severity: Severity::Blocking,
        provenance: None,
        from_path: String::new(),
        to_path: String::new(),
        component_context: None,
        detail: msg.to_string(),
        delta: FindingDelta::Unknown,
    }
}

fn make_external_finding(
    c: &Constraint,
    from_path: &str,
    crate_name: &str,
    via_manifest: bool,
) -> ConstraintFinding {
    let source = if via_manifest {
        "manifest dependency"
    } else {
        "import"
    };
    let detail = match &c.kind {
        ConstraintKind::ForbiddenExternal { from, crates, .. } => format!(
            "forbidden external crate: {from_path} -> {crate_name} via {source} (rule: {from} forbids [{}])",
            crates.join(", ")
        ),
        ConstraintKind::ConfinedExternal { allowed_in, .. } => format!(
            "external crate outside confinement: {from_path} -> {crate_name} via {source} (allowed only in [{}])",
            allowed_in.join(", ")
        ),
        _ => format!("{}: {from_path} -> {crate_name}", c.kind.kind_tag()),
    };
    ConstraintFinding {
        constraint_id: c.id.clone(),
        constraint_name: c.name.clone(),
        constraint_kind: c.kind.kind_tag().to_string(),
        severity: c.severity,
        provenance: c.provenance.clone(),
        from_path: from_path.to_string(),
        to_path: format!("crate:{crate_name}"),
        component_context: None,
        detail,
        delta: FindingDelta::Unknown,
    }
}

/// Check a batch of `(from_path, crate_name)` import items against external
/// constraints. Items come from unresolved import rows (index side) or from
/// parsed proposed content (guard side).
pub fn check_import_items(
    constraints: &[Constraint],
    items: &[(String, String)],
) -> Vec<ConstraintFinding> {
    let mut findings = Vec::new();
    let mut seen: std::collections::HashSet<(String, &str)> = std::collections::HashSet::new();
    for (from_path, crate_name) in items {
        if let Some(c) = match_external(constraints, from_path, crate_name, false) {
            // one finding per (file, crate), not per use-statement
            if seen.insert((from_path.clone(), crate_name.as_str())) {
                findings.push(make_external_finding(c, from_path, crate_name, false));
            }
        }
    }
    findings
}

/// Dependency names declared in a Cargo manifest, split into
/// (normal, dev-and-build). `package = "..."` renames contribute both names.
pub fn cargo_manifest_deps(content: &str) -> (Vec<String>, Vec<String>) {
    let parsed: toml::Value = match content.parse() {
        Ok(v) => v,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let collect = |table: &str| -> Vec<String> {
        let mut names = Vec::new();
        if let Some(deps) = parsed.get(table).and_then(|v| v.as_table()) {
            for (key, val) in deps {
                names.push(key.clone());
                if let Some(pkg) = val.get("package").and_then(|p| p.as_str()) {
                    names.push(pkg.to_string());
                }
            }
        }
        names
    };
    let normal = collect("dependencies");
    let mut dev = collect("dev-dependencies");
    dev.extend(collect("build-dependencies"));
    (normal, dev)
}

/// Check one Cargo manifest's declared dependencies against external constraints.
/// `manifest_rel_path` (e.g. `report/Cargo.toml`) is what `from`/`allowed_in`
/// globs match against.
pub fn check_manifest(
    constraints: &[Constraint],
    manifest_rel_path: &str,
    content: &str,
) -> Vec<ConstraintFinding> {
    let (normal, dev) = cargo_manifest_deps(content);
    let mut findings = Vec::new();
    for (names, is_dev) in [(&normal, false), (&dev, true)] {
        for name in names {
            if let Some(c) = match_external(constraints, manifest_rel_path, name, is_dev) {
                findings.push(make_external_finding(c, manifest_rel_path, name, true));
            }
        }
    }
    findings
}

/// Walk the workspace for Cargo.toml files (depth-limited, skips target/,
/// hidden dirs, node_modules) and return (rel_path, content) pairs.
pub fn scan_workspace_manifests(root: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk_manifests(root, root, 0, &mut out);
    out
}

fn walk_manifests(root: &Path, dir: &Path, depth: usize, out: &mut Vec<(String, String)>) {
    if depth > 5 {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            walk_manifests(root, &path, depth + 1, out);
        } else if name == "Cargo.toml"
            && let (Ok(content), Ok(rel)) =
                (std::fs::read_to_string(&path), path.strip_prefix(root))
        {
            out.push((rel.to_string_lossy().replace('\\', "/"), content));
        }
    }
}

/// Index-side external check: unresolved import rows + workspace manifests.
/// `unresolved` rows are `(file_id, file_path, language, imported_path)`.
/// `changed_ids` (review scope) filters import findings to changed files;
/// manifest findings always pass (manifests are not indexed files).
pub fn check_workspace_externals(
    constraints: &[Constraint],
    workspace_root: &Path,
    unresolved: &[(i64, String, String, String)],
    changed_ids: Option<&std::collections::HashSet<i64>>,
    workspace_crate_names: &[&str],
) -> Vec<ConstraintFinding> {
    if !has_external_constraints(constraints) {
        return Vec::new();
    }
    let mut items: Vec<(String, String)> = Vec::new();
    for (file_id, file_path, language, imported_path) in unresolved {
        if changed_ids.is_some_and(|ids| !ids.contains(file_id)) {
            continue;
        }
        if let Some(name) = external_crate_of_import(imported_path, language, workspace_crate_names)
        {
            items.push((file_path.clone(), name));
        }
    }
    let mut findings = check_import_items(constraints, &items);
    for (rel_path, content) in scan_workspace_manifests(workspace_root) {
        findings.extend(check_manifest(constraints, &rel_path, &content));
    }
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::parse_rules;

    fn constraints_from(toml: &str) -> Vec<Constraint> {
        parse_rules(toml).unwrap().all_constraints().0
    }

    // --- external_crate_of_import ---

    #[test]
    fn rust_external_first_segment() {
        assert_eq!(
            external_crate_of_import("axum::Router", "rust", &[]).as_deref(),
            Some("axum")
        );
        assert_eq!(
            external_crate_of_import("serde", "rust", &[]).as_deref(),
            Some("serde")
        );
        assert_eq!(
            external_crate_of_import("std::collections::HashMap", "rust", &[]).as_deref(),
            Some("std")
        );
    }

    #[test]
    fn rust_internal_imports_rejected() {
        assert_eq!(external_crate_of_import("crate::foo", "rust", &[]), None);
        assert_eq!(external_crate_of_import("self::bar", "rust", &[]), None);
        assert_eq!(external_crate_of_import("super::baz", "rust", &[]), None);
        assert_eq!(
            external_crate_of_import("my_crate::foo", "rust", &["my_crate"]),
            None
        );
    }

    #[test]
    fn rust_workspace_members_excluded() {
        assert_eq!(
            external_crate_of_import("vidya_core::query", "rust", &["vidya", "vidya_core"]),
            None
        );
        assert_eq!(
            external_crate_of_import("vidya::format", "rust", &["vidya", "vidya_core"]),
            None
        );
        assert_eq!(
            external_crate_of_import("serde::Deserialize", "rust", &["vidya", "vidya_core"])
                .as_deref(),
            Some("serde")
        );
    }

    #[test]
    fn rust_glob_suffix_stripped() {
        assert_eq!(
            external_crate_of_import("tokio::sync::*", "rust", &[]).as_deref(),
            Some("tokio")
        );
    }

    #[test]
    fn dart_package_and_sdk_imports() {
        assert_eq!(
            external_crate_of_import("package:flutter/material.dart", "dart", &[]).as_deref(),
            Some("flutter")
        );
        assert_eq!(
            external_crate_of_import("dart:io", "dart", &[]).as_deref(),
            Some("dart:io")
        );
        assert_eq!(
            external_crate_of_import("../widgets/card.dart", "dart", &[]),
            None
        );
    }

    // --- match_external ---

    const FORBID: &str = r#"
[[constraint]]
kind = "forbidden_external"
from = "report/**"
crates = ["axum", "sqlx", "stripe*"]
name = "report-stays-pure"
"#;

    #[test]
    fn forbidden_external_matches_in_scope() {
        let cs = constraints_from(FORBID);
        assert!(match_external(&cs, "report/src/lib.rs", "axum", false).is_some());
        assert!(match_external(&cs, "report/src/render.rs", "stripe_rust", false).is_some());
        assert!(match_external(&cs, "server/src/main.rs", "axum", false).is_none());
        assert!(match_external(&cs, "report/src/lib.rs", "typst", false).is_none());
    }

    #[test]
    fn forbidden_external_default_scope_is_whole_workspace() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
crates = ["arrow_core"]
name = "agpl-boundary"
"#,
        );
        assert!(match_external(&cs, "server/src/main.rs", "arrow_core", false).is_some());
        assert!(match_external(&cs, "Cargo.toml", "arrow_core", false).is_some());
    }

    #[test]
    fn hyphen_underscore_equivalence() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
crates = ["arrow-core"]
"#,
        );
        assert!(match_external(&cs, "src/main.rs", "arrow_core", false).is_some());
    }

    const CONFINE: &str = r#"
[[constraint]]
kind = "confined_external"
crates = ["tonic", "prost"]
allowed_in = ["quiver-client/**"]
name = "protos-only-in-quiver-client"
"#;

    #[test]
    fn confined_external_blocks_outside_allowed_paths() {
        let cs = constraints_from(CONFINE);
        assert!(match_external(&cs, "server/src/main.rs", "tonic", false).is_some());
        assert!(match_external(&cs, "quiver-client/src/lib.rs", "tonic", false).is_none());
        assert!(match_external(&cs, "server/src/main.rs", "axum", false).is_none());
    }

    #[test]
    fn dev_deps_exempt_unless_include_dev() {
        let cs = constraints_from(FORBID);
        assert!(match_external(&cs, "report/src/lib.rs", "axum", true).is_none());

        let strict = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
from = "report/**"
crates = ["axum"]
include_dev = true
"#,
        );
        assert!(match_external(&strict, "report/src/lib.rs", "axum", true).is_some());
    }

    // --- manifests ---

    #[test]
    fn manifest_deps_split_and_renames() {
        let manifest = r#"
[package]
name = "report"

[dependencies]
typst = "0.12"
renamed = { package = "actual-name", version = "1" }
ws-dep = { workspace = true }

[dev-dependencies]
insta = "1"

[build-dependencies]
cc = "1"
"#;
        let (normal, dev) = cargo_manifest_deps(manifest);
        assert!(normal.contains(&"typst".to_string()));
        assert!(normal.contains(&"renamed".to_string()));
        assert!(normal.contains(&"actual-name".to_string()));
        assert!(normal.contains(&"ws-dep".to_string()));
        assert!(dev.contains(&"insta".to_string()));
        assert!(dev.contains(&"cc".to_string()));
    }

    #[test]
    fn manifest_check_flags_forbidden_dep() {
        let cs = constraints_from(FORBID);
        let manifest = "[dependencies]\naxum = \"0.8\"\n";
        let findings = check_manifest(&cs, "report/Cargo.toml", manifest);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].to_path, "crate:axum");
        assert!(findings[0].detail.contains("manifest dependency"));

        let clean = check_manifest(&cs, "server/Cargo.toml", manifest);
        assert!(clean.is_empty());
    }

    #[test]
    fn manifest_check_rename_catches_real_package() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
crates = ["arrow-core"]
"#,
        );
        let manifest = "[dependencies]\ninnocent = { package = \"arrow-core\", version = \"1\" }\n";
        let findings = check_manifest(&cs, "server/Cargo.toml", manifest);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn manifest_dev_deps_exempt_by_default() {
        let cs = constraints_from(FORBID);
        let manifest = "[dev-dependencies]\naxum = \"0.8\"\n";
        let findings = check_manifest(&cs, "report/Cargo.toml", manifest);
        assert!(findings.is_empty());
    }

    // --- import items dedup ---

    #[test]
    fn one_finding_per_file_crate_pair() {
        let cs = constraints_from(FORBID);
        let items = vec![
            ("report/src/lib.rs".to_string(), "axum".to_string()),
            ("report/src/lib.rs".to_string(), "axum".to_string()),
            ("report/src/render.rs".to_string(), "axum".to_string()),
        ];
        let findings = check_import_items(&cs, &items);
        assert_eq!(findings.len(), 2);
    }

    // --- validate_no_external_targeting_members ---

    #[test]
    fn validate_rejects_external_targeting_member() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
crates = ["vidya_core"]
name = "no-vidya-core"
"#,
        );
        let err =
            validate_no_external_targeting_members(&cs, &["vidya", "vidya_core"]).unwrap_err();
        assert!(err.contains("no-vidya-core"));
        assert!(err.contains("vidya_core"));
        assert!(err.contains("forbidden_dep"));
    }

    #[test]
    fn validate_passes_truly_external() {
        let cs = constraints_from(FORBID);
        assert!(validate_no_external_targeting_members(&cs, &["vidya", "vidya_core"]).is_ok());
    }

    #[test]
    fn validate_passes_empty_workspace() {
        let cs = constraints_from(FORBID);
        assert!(validate_no_external_targeting_members(&cs, &[]).is_ok());
    }

    // --- config_error_finding ---

    #[test]
    fn config_error_finding_is_blocking() {
        let f = config_error_finding("bad config");
        assert_eq!(f.severity, Severity::Blocking);
        assert_eq!(f.constraint_kind, "config_error");
        assert!(f.detail.contains("bad config"));
    }
}
