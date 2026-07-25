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
use std::sync::Arc;

use glob::{MatchOptions, Pattern};

use crate::constraints::{ConstraintFinding, FindingDelta};
use crate::db::UnresolvedImport;
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
            let path = raw_path.strip_prefix("::").unwrap_or(raw_path);
            let path = path.strip_suffix("::*").unwrap_or(path);
            let first = path.split("::").next()?.trim();
            if first.is_empty() || matches!(first, "crate" | "self" | "super") {
                return None;
            }
            if workspace_crate_names.contains(&first) {
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
    match_external_where(constraints, from_path, crate_name, is_dev, &|_| true)
}

/// `match_external` restricted to constraints the caller considers applicable.
///
/// Matching is first-match — exactly one constraint reports per `(file, crate)`
/// — so applicability has to be part of *matching*, not a filter after it.
/// Filtering afterwards lets a broad rule win the match and then discard the
/// item, shadowing a narrower rule that would have fired (sutra/296).
pub fn match_external_where<'a>(
    constraints: &'a [Constraint],
    from_path: &str,
    crate_name: &str,
    is_dev: bool,
    applicable: &dyn Fn(&Constraint) -> bool,
) -> Option<&'a Constraint> {
    constraints
        .iter()
        .filter(|c| applicable(c))
        .find(|c| match &c.kind {
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

/// Directory holding a manifest, `""` for the workspace-root manifest.
fn manifest_dir(manifest_rel_path: &str) -> &str {
    match manifest_rel_path.rfind('/') {
        Some(i) => &manifest_rel_path[..i],
        None => "",
    }
}

/// Whether `path` lies under `dir`. The empty dir is the workspace root and
/// contains everything.
fn dir_contains(dir: &str, path: &str) -> bool {
    if dir.is_empty() {
        return true;
    }
    path.strip_prefix(dir)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// How far into `dir` an `allowed_in` pattern can reach.
struct Reach {
    /// The pattern can match at least one path inside `dir`.
    inside: bool,
    /// A `**` gives the pattern a match *directly* inside `dir` at any depth, so
    /// no nested package can claim the path exclusively.
    at_any_depth: bool,
}

impl Reach {
    const OUTSIDE: Reach = Reach {
        inside: false,
        at_any_depth: false,
    };
}

/// Whether a path glob can match inside `dir`, decided component by component.
///
/// Prefix arithmetic on the glob's literal head is not enough: for
/// `crates/*/src/db.rs` the literal head is `crates/`, which no package directory
/// equals, so a literal-prefix rule assigns the path to the workspace root while
/// leaving the member that actually holds it unowned — exactly inverting the
/// exemption. Aligning components resolves the wildcard segment against real
/// package directories instead.
fn pattern_reach(pattern: &str, dir: &str) -> Reach {
    let pat: Vec<&str> = pattern
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let mut next = 0;
    for component in dir.split('/').filter(|c| !c.is_empty()) {
        match pat.get(next) {
            // The pattern is shallower than the directory: it names something
            // above this package, never inside it.
            None => return Reach::OUTSIDE,
            Some(&"**") => {
                return Reach {
                    inside: true,
                    at_any_depth: true,
                };
            }
            Some(glob) => {
                if !Pattern::new(glob)
                    .ok()
                    .is_some_and(|p| p.matches(component))
                {
                    return Reach::OUTSIDE;
                }
                next += 1;
            }
        }
    }
    Reach {
        inside: true,
        at_any_depth: matches!(pat.get(next), Some(&"**")),
    }
}

/// Whether the package declaring `manifest_rel_path` owns at least one of the
/// confinement paths — i.e. `allowed_in` points at that package's own sources.
///
/// For such a package the manifest entry is *how* the dependency reaches those
/// files, so flagging it makes the constraint unsatisfiable: `Cargo.toml` can
/// never itself be listed in `allowed_in` (sutra/291). The signal stays live for
/// a non-owning manifest, which is the case it was written for — crate A
/// declaring a dependency only crate B may use.
///
/// `package_dirs` lists every package directory in the workspace, so a nested
/// package the pattern also reaches takes the path instead of the enclosing one.
/// Ambiguity resolves toward *not* owning: wrongly exempting a manifest silently
/// disables a blocking rule, while wrongly reporting one is visible and waivable.
/// Pass an empty slice when the package set is unknown; ownership then rests on
/// containment alone.
fn manifest_owns_confinement(
    manifest_rel_path: &str,
    allowed_in: &[String],
    package_dirs: &[&str],
) -> bool {
    let own_dir = manifest_dir(manifest_rel_path);
    allowed_in.iter().any(|pattern| {
        let reach = pattern_reach(pattern, own_dir);
        if !reach.inside {
            return false;
        }
        // `**` reaches this package's own files whatever else it also covers.
        if reach.at_any_depth {
            return true;
        }
        !package_dirs.iter().any(|nested| {
            nested.len() > own_dir.len()
                && dir_contains(own_dir, nested)
                && pattern_reach(pattern, nested).inside
        })
    })
}

/// Manifest paths (`report/Cargo.toml`) reduced to the package directories that
/// hold them, with the root manifest becoming `""`. Deduplicated and sorted.
pub fn package_dirs_of<'a>(manifest_paths: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    manifest_paths
        .into_iter()
        .map(manifest_dir)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Package directories for the manifest kind matching `manifest_rel_path`,
/// including that path itself — a proposed manifest may not be on disk yet.
///
/// Shared by the guard and the index so both derive the same package layout.
/// Deriving the guard's view from `[workspace].members` instead let a nested
/// non-member package shift ownership between the two, so the guard would allow
/// an edit the next audit reported.
pub fn package_dirs_including(root: &Path, manifest_rel_path: &str) -> Vec<String> {
    let project_files = scan_project_files(root);
    let mut paths: Vec<String> = if manifest_rel_path.ends_with("pubspec.yaml") {
        project_files.pubspecs
    } else {
        project_files.manifests
    }
    .into_iter()
    .map(|(rel, _)| rel)
    .collect();
    if !paths.iter().any(|p| p == manifest_rel_path) {
        paths.push(manifest_rel_path.to_string());
    }
    paths
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
        constraint_id: Arc::from("config-error"),
        constraint_name: None,
        constraint_kind: "config_error".to_string(),
        severity: Severity::Blocking,
        provenance: None,
        from_path: String::new(),
        to_path: String::new(),
        component_context: None,
        detail: msg.to_string(),
        delta: FindingDelta::Unknown,
        line: None,
        snippet: None,
        enclosing_symbol: None,
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
        line: None,
        snippet: None,
        enclosing_symbol: None,
    }
}

/// Check a batch of `(from_path, crate_name, is_test)` import items against
/// external constraints. Items come from unresolved import rows (index side) or
/// from parsed proposed content (guard side).
///
/// A crate reached only from `#[cfg(test)]` code is not a production dependency,
/// so test items are matched only against constraints that want them: those
/// opting in with `include_tests` (sutra/294), and those aiming themselves at a
/// test path via `from` or `scope` (sutra/296).
pub fn check_import_items(
    constraints: &[Constraint],
    items: &[(String, String, bool)],
) -> Vec<ConstraintFinding> {
    let mut findings = Vec::new();
    let mut seen: std::collections::HashSet<(String, &str)> = std::collections::HashSet::new();
    let wants_tests: std::collections::HashSet<&str> = constraints
        .iter()
        .filter(|c| {
            c.include_tests
                || super::constraint_targets_tests(
                    c,
                    &crate::parser::adapter::any_language_is_test_path,
                )
        })
        .map(|c| c.id.as_ref())
        .collect();
    for (from_path, crate_name, is_test) in items {
        let applicable = |c: &Constraint| !*is_test || wants_tests.contains(c.id.as_ref());
        if let Some(c) =
            match_external_where(constraints, from_path, crate_name, false, &applicable)
        {
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
/// When `ws_renames` is provided, `workspace = true` entries are resolved
/// through the root manifest's rename map.
pub fn cargo_manifest_deps(
    content: &str,
    ws_renames: Option<&std::collections::HashMap<String, String>>,
) -> (Vec<String>, Vec<String>) {
    let parsed: toml::Value = match content.parse() {
        Ok(v) => v,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let root_table = match parsed.as_table() {
        Some(t) => t,
        None => return (Vec::new(), Vec::new()),
    };
    let collect_from = |table: &toml::value::Table, key: &str| -> Vec<String> {
        let mut names = Vec::new();
        if let Some(deps) = table.get(key).and_then(|v| v.as_table()) {
            for (dep_key, val) in deps {
                names.push(dep_key.clone());
                if let Some(pkg) = val.get("package").and_then(|p| p.as_str()) {
                    names.push(pkg.to_string());
                } else if val
                    .get("workspace")
                    .and_then(|w| w.as_bool())
                    .unwrap_or(false)
                    && let Some(real) = ws_renames.and_then(|m| m.get(dep_key))
                {
                    names.push(real.clone());
                }
            }
        }
        names
    };
    let mut normal = collect_from(root_table, "dependencies");
    let mut dev = collect_from(root_table, "dev-dependencies");
    dev.extend(collect_from(root_table, "build-dependencies"));

    if let Some(targets) = root_table.get("target").and_then(|v| v.as_table()) {
        for (_spec, target_val) in targets {
            if let Some(target_table) = target_val.as_table() {
                normal.extend(collect_from(target_table, "dependencies"));
                dev.extend(collect_from(target_table, "dev-dependencies"));
                dev.extend(collect_from(target_table, "build-dependencies"));
            }
        }
    }

    (normal, dev)
}

/// Build alias → real-package-name map from a root Cargo.toml's
/// `[workspace.dependencies]` entries that have `package = "..."`.
pub fn workspace_dep_renames(root_content: &str) -> std::collections::HashMap<String, String> {
    let parsed: toml::Value = match root_content.parse() {
        Ok(v) => v,
        Err(_) => return std::collections::HashMap::new(),
    };
    let mut map = std::collections::HashMap::new();
    if let Some(deps) = parsed
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        for (alias, val) in deps {
            if let Some(pkg) = val.get("package").and_then(|p| p.as_str()) {
                map.insert(alias.clone(), pkg.to_string());
            }
        }
    }
    map
}

/// Applicability predicate for manifest checks: a `confined_external` whose
/// `allowed_in` names this package's own sources does not apply to the package's
/// own dependency declaration (sutra/291).
///
/// Expressed as applicability rather than a post-filter because external
/// matching is first-match — filtering afterwards would let the exempt rule win
/// the match and shadow a narrower rule that should have fired (sutra/296).
fn manifest_applicable<'p>(
    manifest_rel_path: &'p str,
    package_dirs: &'p [&'p str],
) -> impl Fn(&Constraint) -> bool + 'p {
    move |c: &Constraint| match &c.kind {
        ConstraintKind::ConfinedExternal { allowed_in, .. } => {
            !manifest_owns_confinement(manifest_rel_path, allowed_in, package_dirs)
        }
        _ => true,
    }
}

/// Check one Cargo manifest's declared dependencies against external constraints.
/// `manifest_rel_path` (e.g. `report/Cargo.toml`) is what `from`/`allowed_in`
/// globs match against. `package_dirs` is every package directory in the
/// workspace (see `manifest_owns_confinement`); an empty slice is acceptable.
pub fn check_manifest(
    constraints: &[Constraint],
    manifest_rel_path: &str,
    content: &str,
    ws_renames: Option<&std::collections::HashMap<String, String>>,
    package_dirs: &[&str],
) -> Vec<ConstraintFinding> {
    let (normal, dev) = cargo_manifest_deps(content, ws_renames);
    let applicable = manifest_applicable(manifest_rel_path, package_dirs);
    let mut findings = Vec::new();
    for (names, is_dev) in [(&normal, false), (&dev, true)] {
        for name in names {
            if let Some(c) =
                match_external_where(constraints, manifest_rel_path, name, is_dev, &applicable)
            {
                findings.push(make_external_finding(c, manifest_rel_path, name, true));
            }
        }
    }
    findings
}

/// Dependency names declared in a pubspec.yaml, split into (normal, dev).
pub fn pubspec_deps(content: &str) -> (Vec<String>, Vec<String>) {
    use yaml_rust2::YamlLoader;

    let docs = match YamlLoader::load_from_str(content) {
        Ok(d) => d,
        Err(_) => return (Vec::new(), Vec::new()),
    };
    let doc = match docs.first() {
        Some(d) => d,
        None => return (Vec::new(), Vec::new()),
    };
    let collect_keys = |key: &str| -> Vec<String> {
        doc[key]
            .as_hash()
            .map(|h| {
                h.keys()
                    .filter_map(|k| k.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    let normal = collect_keys("dependencies");
    let dev = collect_keys("dev_dependencies");
    (normal, dev)
}

/// Check one pubspec.yaml's declared dependencies against external constraints.
/// `package_dirs` carries the same confinement-ownership role as in
/// `check_manifest`.
pub fn check_pubspec(
    constraints: &[Constraint],
    pubspec_rel_path: &str,
    content: &str,
    package_dirs: &[&str],
) -> Vec<ConstraintFinding> {
    let (normal, dev) = pubspec_deps(content);
    let applicable = manifest_applicable(pubspec_rel_path, package_dirs);
    let mut findings = Vec::new();
    for (names, is_dev) in [(&normal, false), (&dev, true)] {
        for name in names {
            if let Some(c) =
                match_external_where(constraints, pubspec_rel_path, name, is_dev, &applicable)
            {
                findings.push(make_external_finding(c, pubspec_rel_path, name, true));
            }
        }
    }
    findings
}

pub struct ProjectFiles {
    pub manifests: Vec<(String, String)>,
    pub pubspecs: Vec<(String, String)>,
}

/// Walk the workspace for Cargo.toml and pubspec.yaml files (depth-limited,
/// skips target/, hidden dirs, node_modules, build/) and return grouped results.
pub fn scan_project_files(root: &Path) -> ProjectFiles {
    let mut pf = ProjectFiles {
        manifests: Vec::new(),
        pubspecs: Vec::new(),
    };
    walk_project_files(root, root, 0, &mut pf);
    pf
}

/// Walk the workspace for Cargo.toml files (depth-limited, skips target/,
/// hidden dirs, node_modules) and return (rel_path, content) pairs.
pub fn scan_workspace_manifests(root: &Path) -> Vec<(String, String)> {
    scan_project_files(root).manifests
}

/// Read member Cargo.toml files for declared `[workspace].members` in the
/// given root content. Only returns manifests for members that exist on disk.
pub fn workspace_member_manifests(root: &Path, root_content: &str) -> Vec<(String, String)> {
    let parsed: toml::Value = match root_content.parse() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let member_globs: Vec<&str> = parsed
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for pattern in member_globs {
        let abs_pattern = root.join(pattern);
        let Ok(paths) = glob::glob(&abs_pattern.to_string_lossy()) else {
            continue;
        };
        for entry in paths.flatten() {
            let cargo = entry.join("Cargo.toml");
            if let (Ok(content), Ok(rel)) =
                (std::fs::read_to_string(&cargo), cargo.strip_prefix(root))
            {
                out.push((rel.to_string_lossy().replace('\\', "/"), content));
            }
        }
    }
    out
}

fn walk_project_files(root: &Path, dir: &Path, depth: usize, out: &mut ProjectFiles) {
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
            if name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == "build"
                || name == ".dart_tool"
            {
                continue;
            }
            walk_project_files(root, &path, depth + 1, out);
        } else if (*name == *"Cargo.toml" || *name == *"pubspec.yaml")
            && let (Ok(content), Ok(rel)) =
                (std::fs::read_to_string(&path), path.strip_prefix(root))
        {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if *name == *"Cargo.toml" {
                out.manifests.push((rel_str, content));
            } else {
                out.pubspecs.push((rel_str, content));
            }
        }
    }
}

/// Index-side external check: unresolved import rows + workspace manifests.
/// `unresolved` rows come straight from `Db::unresolved_imports_with_files`.
/// `changed_ids` (review scope) filters import findings to changed files;
/// manifest findings always pass (manifests are not indexed files).
pub fn check_workspace_externals(
    constraints: &[Constraint],
    workspace_root: &Path,
    unresolved: &[UnresolvedImport],
    changed_ids: Option<&std::collections::HashSet<i64>>,
    workspace_crate_names: &[&str],
) -> Vec<ConstraintFinding> {
    if !has_external_constraints(constraints) {
        return Vec::new();
    }
    let mut items: Vec<(String, String, bool)> = Vec::new();
    for import in unresolved {
        if changed_ids.is_some_and(|ids| !ids.contains(&import.file_id)) {
            continue;
        }
        if let Some(name) = external_crate_of_import(
            &import.imported_path,
            &import.language,
            workspace_crate_names,
        ) {
            items.push((import.path.clone(), name, import.is_test));
        }
    }
    let project_files = scan_project_files(workspace_root);
    let ws_renames = project_files
        .manifests
        .iter()
        .find(|(rel, _)| rel == "Cargo.toml")
        .map(|(_, content)| workspace_dep_renames(content))
        .unwrap_or_default();
    let renames = if ws_renames.is_empty() {
        None
    } else {
        Some(&ws_renames)
    };
    if !ws_renames.is_empty() {
        let mut extra = Vec::new();
        for (path, name, is_test) in &items {
            if let Some(real) = ws_renames.get(name) {
                extra.push((path.clone(), real.clone(), *is_test));
            }
        }
        items.extend(extra);
    }
    let cargo_dirs = package_dirs_of(project_files.manifests.iter().map(|(rel, _)| rel.as_str()));
    let pubspec_dirs = package_dirs_of(project_files.pubspecs.iter().map(|(rel, _)| rel.as_str()));
    let mut findings = check_import_items(constraints, &items);
    for (rel_path, content) in &project_files.manifests {
        findings.extend(check_manifest(
            constraints,
            rel_path,
            content,
            renames,
            &cargo_dirs,
        ));
    }
    for (rel_path, content) in &project_files.pubspecs {
        findings.extend(check_pubspec(constraints, rel_path, content, &pubspec_dirs));
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
    fn rust_leading_colons_stripped() {
        assert_eq!(
            external_crate_of_import("::axum::Router", "rust", &[]).as_deref(),
            Some("axum")
        );
        assert_eq!(
            external_crate_of_import("::serde", "rust", &[]).as_deref(),
            Some("serde")
        );
        assert_eq!(
            external_crate_of_import("::tokio::sync::*", "rust", &[]).as_deref(),
            Some("tokio")
        );
        assert_eq!(
            external_crate_of_import("::vidya_core::query", "rust", &["vidya_core"]),
            None
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
        let (normal, dev) = cargo_manifest_deps(manifest, None);
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
        let findings = check_manifest(&cs, "report/Cargo.toml", manifest, None, &[]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].to_path, "crate:axum");
        assert!(findings[0].detail.contains("manifest dependency"));

        let clean = check_manifest(&cs, "server/Cargo.toml", manifest, None, &[]);
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
        let findings = check_manifest(&cs, "server/Cargo.toml", manifest, None, &[]);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn manifest_dev_deps_exempt_by_default() {
        let cs = constraints_from(FORBID);
        let manifest = "[dev-dependencies]\naxum = \"0.8\"\n";
        let findings = check_manifest(&cs, "report/Cargo.toml", manifest, None, &[]);
        assert!(findings.is_empty());
    }

    #[test]
    fn manifest_target_deps_collected() {
        let manifest = r#"
[package]
name = "myapp"

[dependencies]
serde = "1"

[target.'cfg(unix)'.dependencies]
inotify = "0.10"

[target.'cfg(windows)'.dependencies]
winapi = { package = "windows-sys", version = "0.52" }

[target.'cfg(unix)'.dev-dependencies]
nix = "0.29"
"#;
        let (normal, dev) = cargo_manifest_deps(manifest, None);
        assert!(normal.contains(&"serde".to_string()));
        assert!(normal.contains(&"inotify".to_string()));
        assert!(normal.contains(&"winapi".to_string()));
        assert!(normal.contains(&"windows-sys".to_string()));
        assert!(dev.contains(&"nix".to_string()));
    }

    #[test]
    fn workspace_dep_renames_extracts_package_field() {
        let root = r#"
[workspace]
members = ["server"]

[workspace.dependencies]
innocent = { package = "arrow-core", version = "1" }
serde = "1"
"#;
        let renames = workspace_dep_renames(root);
        assert_eq!(
            renames.get("innocent").map(|s| s.as_str()),
            Some("arrow-core")
        );
        assert!(!renames.contains_key("serde"));
    }

    #[test]
    fn manifest_workspace_true_resolves_rename() {
        let renames =
            std::collections::HashMap::from([("innocent".to_string(), "arrow-core".to_string())]);
        let manifest = r#"
[package]
name = "server"

[dependencies]
innocent = { workspace = true }
serde = { workspace = true }
"#;
        let (normal, _dev) = cargo_manifest_deps(manifest, Some(&renames));
        assert!(normal.contains(&"innocent".to_string()));
        assert!(normal.contains(&"arrow-core".to_string()));
        assert!(normal.contains(&"serde".to_string()));
    }

    #[test]
    fn manifest_check_catches_workspace_rename() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
crates = ["arrow-core"]
"#,
        );
        let renames =
            std::collections::HashMap::from([("innocent".to_string(), "arrow-core".to_string())]);
        let manifest = "[dependencies]\ninnocent = { workspace = true }\n";
        let findings = check_manifest(&cs, "server/Cargo.toml", manifest, Some(&renames), &[]);
        assert_eq!(findings.len(), 1);
    }

    // --- confinement ownership (sutra/291) ---

    const CONFINE_SINGLE: &str = r#"
[[constraint]]
kind = "confined_external"
crates = ["rusqlite"]
allowed_in = ["src/db.rs", "src/error.rs"]
name = "sqlite-single-point-of-contact"
"#;

    #[test]
    fn single_crate_manifest_is_exempt_from_own_confinement() {
        let cs = constraints_from(CONFINE_SINGLE);
        let manifest = "[package]\nname = \"yojana\"\n\n[dependencies]\nrusqlite = \"0.32\"\n";
        let findings = check_manifest(&cs, "Cargo.toml", manifest, None, &[""]);
        assert!(
            findings.is_empty(),
            "the declaring package's own manifest can never appear in allowed_in, \
             so flagging it makes the constraint unsatisfiable, got: {:?}",
            findings.iter().map(|f| &f.detail).collect::<Vec<_>>()
        );
    }

    #[test]
    fn confinement_import_signal_survives_manifest_exemption() {
        // The exemption is manifest-only: a use-statement outside allowed_in is
        // still the violation the rule exists to catch.
        let cs = constraints_from(CONFINE_SINGLE);
        let items = vec![("src/server.rs".to_string(), "rusqlite".to_string(), false)];
        assert_eq!(check_import_items(&cs, &items).len(), 1);
        let allowed = vec![("src/db.rs".to_string(), "rusqlite".to_string(), false)];
        assert!(check_import_items(&cs, &allowed).is_empty());
    }

    #[test]
    fn non_owning_workspace_member_manifest_still_flagged() {
        let cs = constraints_from(CONFINE);
        let manifest = "[dependencies]\ntonic = \"0.12\"\n";
        let dirs = ["", "quiver-client", "server"];
        let findings = check_manifest(&cs, "server/Cargo.toml", manifest, None, &dirs);
        assert_eq!(
            findings.len(),
            1,
            "a member declaring a dependency confined to another member is the \
             case the manifest signal was written for"
        );
        assert_eq!(findings[0].to_path, "crate:tonic");

        let owner = check_manifest(&cs, "quiver-client/Cargo.toml", manifest, None, &dirs);
        assert!(owner.is_empty(), "the owning member is exempt");
    }

    #[test]
    fn root_package_not_exempt_when_a_member_owns_the_confinement_path() {
        // The root dir contains everything, so ownership goes to the deepest
        // package declaring the path — otherwise the root would swallow it.
        let cs = constraints_from(CONFINE);
        let manifest = "[package]\nname = \"top\"\n\n[dependencies]\ntonic = \"0.12\"\n";
        let findings = check_manifest(&cs, "Cargo.toml", manifest, None, &["", "quiver-client"]);
        assert_eq!(findings.len(), 1);
    }

    #[test]
    fn exempt_confinement_does_not_shadow_a_forbidden_external_rule() {
        // Matching is first-match: the exempt confined rule must step out of the
        // way rather than win the match and discard the finding (sutra/296).
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "confined_external"
crates = ["rusqlite"]
allowed_in = ["src/db.rs"]
name = "confine-sqlite"

[[constraint]]
kind = "forbidden_external"
crates = ["rusqlite"]
name = "no-sqlite-at-all"
"#,
        );
        let manifest = "[dependencies]\nrusqlite = \"0.32\"\n";
        let findings = check_manifest(&cs, "Cargo.toml", manifest, None, &[""]);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].constraint_name.as_deref(),
            Some("no-sqlite-at-all")
        );
    }

    #[test]
    fn leading_double_star_reaches_every_package_including_the_declarer() {
        // `**/client.rs` permits a client.rs anywhere, including in the
        // declaring package — so its manifest is exempt. Reading only the
        // literal prefix (empty here) would call that "no package" and recreate
        // the unsatisfiable rule this fix exists to remove.
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "confined_external"
crates = ["tonic"]
allowed_in = ["**/client.rs"]
"#,
        );
        let manifest = "[dependencies]\ntonic = \"0.12\"\n";
        assert!(check_manifest(&cs, "Cargo.toml", manifest, None, &[""]).is_empty());
        assert!(
            check_manifest(&cs, "server/Cargo.toml", manifest, None, &["", "server"]).is_empty(),
            "a member's own client.rs is permitted too"
        );
    }

    #[test]
    fn wildcard_member_segment_resolves_to_the_member_not_the_root() {
        // `crates/*/src/db.rs` has literal head `crates/`, which is no package
        // directory. Prefix arithmetic hands ownership to the root (exempting a
        // manifest that should report) and leaves the member that actually holds
        // the path unowned (reporting a manifest that can never be fixed) —
        // exactly inverted. Component alignment resolves `*` to `api`.
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "confined_external"
crates = ["rusqlite"]
allowed_in = ["crates/*/src/db.rs"]
"#,
        );
        let manifest = "[dependencies]\nrusqlite = \"0.32\"\n";
        let dirs = ["", "crates/api"];
        assert_eq!(
            check_manifest(&cs, "Cargo.toml", manifest, None, &dirs).len(),
            1,
            "the root does not own a path confined to a member"
        );
        assert!(
            check_manifest(&cs, "crates/api/Cargo.toml", manifest, None, &dirs).is_empty(),
            "the member the wildcard resolves to owns the path"
        );
        assert_eq!(
            check_manifest(&cs, "crates/cli/Cargo.toml", manifest, None, &dirs).len(),
            0,
            "any crates/* member matches the wildcard segment"
        );
    }

    #[test]
    fn sibling_dir_with_a_shared_name_prefix_is_not_confused_for_a_parent() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "confined_external"
crates = ["tonic"]
allowed_in = ["report/src/**"]
"#,
        );
        let manifest = "[dependencies]\ntonic = \"0.12\"\n";
        let dirs = ["", "report", "report-core"];
        assert_eq!(
            check_manifest(&cs, "report-core/Cargo.toml", manifest, None, &dirs).len(),
            1,
            "report-core is a sibling of report, not its owner"
        );
        assert!(check_manifest(&cs, "report/Cargo.toml", manifest, None, &dirs).is_empty());
    }

    #[test]
    fn nested_package_takes_ownership_from_its_parent_package() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "confined_external"
crates = ["tonic"]
allowed_in = ["server/nested/src/rpc.rs"]
"#,
        );
        let manifest = "[dependencies]\ntonic = \"0.12\"\n";
        let dirs = ["", "server", "server/nested"];
        assert_eq!(
            check_manifest(&cs, "server/Cargo.toml", manifest, None, &dirs).len(),
            1
        );
        assert!(check_manifest(&cs, "server/nested/Cargo.toml", manifest, None, &dirs).is_empty());
    }

    #[test]
    fn allowed_in_naming_the_package_dir_itself_is_ownership() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "confined_external"
crates = ["tonic"]
allowed_in = ["server"]
"#,
        );
        let manifest = "[dependencies]\ntonic = \"0.12\"\n";
        let dirs = ["", "server"];
        assert!(check_manifest(&cs, "server/Cargo.toml", manifest, None, &dirs).is_empty());
        assert_eq!(
            check_manifest(&cs, "Cargo.toml", manifest, None, &dirs).len(),
            1
        );
    }

    #[test]
    fn leading_dot_slash_in_allowed_in_still_grants_ownership() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "confined_external"
crates = ["rusqlite"]
allowed_in = ["./src/db.rs"]
"#,
        );
        let manifest = "[dependencies]\nrusqlite = \"0.32\"\n";
        assert!(check_manifest(&cs, "Cargo.toml", manifest, None, &[""]).is_empty());
    }

    #[test]
    fn single_package_pubspec_is_exempt_from_own_confinement() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "confined_external"
crates = ["http"]
allowed_in = ["lib/net/**"]
"#,
        );
        let pubspec = "name: my_app\n\ndependencies:\n  http: ^1.0.0\n";
        assert!(check_pubspec(&cs, "pubspec.yaml", pubspec, &[""]).is_empty());
        assert_eq!(
            check_pubspec(&cs, "other/pubspec.yaml", pubspec, &["", "other"]).len(),
            1
        );
    }

    #[test]
    fn package_dirs_of_reduces_manifest_paths() {
        let paths = [
            "Cargo.toml",
            "server/Cargo.toml",
            "crates/report/Cargo.toml",
            "server/Cargo.toml",
        ];
        assert_eq!(
            package_dirs_of(paths.iter().copied()),
            vec!["", "crates/report", "server"]
        );
    }

    // --- import items dedup ---

    #[test]
    fn one_finding_per_file_crate_pair() {
        let cs = constraints_from(FORBID);
        let items = vec![
            ("report/src/lib.rs".to_string(), "axum".to_string(), false),
            ("report/src/lib.rs".to_string(), "axum".to_string(), false),
            (
                "report/src/render.rs".to_string(),
                "axum".to_string(),
                false,
            ),
        ];
        let findings = check_import_items(&cs, &items);
        assert_eq!(findings.len(), 2);
    }

    // --- test scope (sutra/294) ---

    #[test]
    fn crate_used_only_from_test_scope_is_not_a_violation() {
        let cs = constraints_from(FORBID);
        let items = vec![("report/src/lib.rs".to_string(), "axum".to_string(), true)];
        let findings = check_import_items(&cs, &items);
        assert!(
            findings.is_empty(),
            "a crate reached only from #[cfg(test)] is not a production dependency, got: {:?}",
            findings.iter().map(|f| &f.detail).collect::<Vec<_>>()
        );
    }

    #[test]
    fn include_tests_opt_in_restores_external_test_finding() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
from = "report/**"
crates = ["axum"]
name = "report-stays-pure"
include_tests = true
"#,
        );
        assert!(cs[0].include_tests);
        let items = vec![("report/src/lib.rs".to_string(), "axum".to_string(), true)];
        assert_eq!(check_import_items(&cs, &items).len(), 1);
    }

    #[test]
    fn production_use_of_same_crate_still_reported_alongside_test_use() {
        let cs = constraints_from(FORBID);
        let items = vec![
            ("report/src/lib.rs".to_string(), "axum".to_string(), true),
            (
                "report/src/render.rs".to_string(),
                "axum".to_string(),
                false,
            ),
        ];
        let findings = check_import_items(&cs, &items);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].from_path, "report/src/render.rs");
    }

    // --- test-directed escape hatch and rule-order shadowing (sutra/296) ---

    #[test]
    fn external_rule_aimed_at_tests_fires_without_include_tests() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
from = "tests/**"
crates = ["axum"]
name = "no-axum-in-integration-tests"
"#,
        );
        assert!(!cs[0].include_tests);
        let items = vec![("tests/api.rs".to_string(), "axum".to_string(), true)];
        assert_eq!(
            check_import_items(&cs, &items).len(),
            1,
            "a rule written for tests/ must not be muted by test-scope exclusion"
        );
    }

    #[test]
    fn confined_external_allowed_in_tests_is_not_test_directed() {
        // `allowed_in` is an allowlist, not a target: naming tests/ there says
        // test usage is permitted, so it must not opt the rule into test scope.
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "confined_external"
crates = ["axum"]
allowed_in = ["tests/**"]
"#,
        );
        let items = vec![("src/lib.rs".to_string(), "axum".to_string(), true)];
        assert!(
            check_import_items(&cs, &items).is_empty(),
            "allowed_in must not act as a test-directed escape hatch"
        );
    }

    #[test]
    fn broad_rule_does_not_shadow_a_narrower_include_tests_rule() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
from = "**"
crates = ["axum"]
name = "broad-default"

[[constraint]]
kind = "forbidden_external"
from = "report/**"
crates = ["axum"]
name = "report-includes-tests"
include_tests = true
"#,
        );
        let items = vec![("report/src/lib.rs".to_string(), "axum".to_string(), true)];
        let findings = check_import_items(&cs, &items);
        assert_eq!(
            findings.len(),
            1,
            "the opt-in rule must be reached even though a broad rule matches first"
        );
        assert_eq!(
            findings[0].constraint_name.as_deref(),
            Some("report-includes-tests")
        );
    }

    #[test]
    fn applicability_matching_does_not_multiply_findings_for_production_items() {
        // Overlapping rules still report once per (file, crate) — restricting
        // *matching* by applicability must not turn first-match into all-match.
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
from = "**"
crates = ["axum"]
name = "broad-default"

[[constraint]]
kind = "forbidden_external"
from = "report/**"
crates = ["axum"]
name = "narrower"
"#,
        );
        let items = vec![("report/src/lib.rs".to_string(), "axum".to_string(), false)];
        let findings = check_import_items(&cs, &items);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].constraint_name.as_deref(),
            Some("broad-default")
        );
    }

    // --- import rename resolution ---

    #[test]
    fn import_alias_resolved_through_workspace_renames() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
from = "server/src/**"
crates = ["arrow-core"]
"#,
        );
        let items = vec![(
            "server/src/main.rs".to_string(),
            "innocent".to_string(),
            false,
        )];
        let findings_without = check_import_items(&cs, &items);
        assert!(findings_without.is_empty(), "alias alone should not match");

        let mut resolved_items = items.clone();
        let ws_renames =
            std::collections::HashMap::from([("innocent".to_string(), "arrow-core".to_string())]);
        let mut extra = Vec::new();
        for (path, name, is_test) in &resolved_items {
            if let Some(real) = ws_renames.get(name) {
                extra.push((path.clone(), real.clone(), *is_test));
            }
        }
        resolved_items.extend(extra);
        let findings_with = check_import_items(&cs, &resolved_items);
        assert_eq!(findings_with.len(), 1, "real package name should match");
        assert_eq!(findings_with[0].to_path, "crate:arrow-core");
    }

    // --- pubspec ---

    #[test]
    fn pubspec_deps_parses_dependencies() {
        let pubspec = r#"
name: my_app
version: 1.0.0

dependencies:
  flutter:
    sdk: flutter
  http: ^1.0.0
  arrow_core: ^2.0.0

dev_dependencies:
  flutter_test:
    sdk: flutter
  mockito: ^5.0.0
"#;
        let (normal, dev) = pubspec_deps(pubspec);
        assert!(normal.contains(&"flutter".to_string()));
        assert!(normal.contains(&"http".to_string()));
        assert!(normal.contains(&"arrow_core".to_string()));
        assert!(dev.contains(&"flutter_test".to_string()));
        assert!(dev.contains(&"mockito".to_string()));
        assert!(!normal.contains(&"mockito".to_string()));
    }

    #[test]
    fn check_pubspec_flags_forbidden_dep() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
crates = ["arrow_core"]
"#,
        );
        let pubspec = "name: my_app\n\ndependencies:\n  arrow_core: ^2.0.0\n";
        let findings = check_pubspec(&cs, "my_app/pubspec.yaml", pubspec, &[]);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("manifest dependency"));
    }

    #[test]
    fn check_pubspec_dev_deps_exempt_by_default() {
        let cs = constraints_from(
            r#"
[[constraint]]
kind = "forbidden_external"
from = "my_app/**"
crates = ["mockito"]
"#,
        );
        let pubspec = "name: my_app\n\ndev_dependencies:\n  mockito: ^5.0.0\n";
        let findings = check_pubspec(&cs, "my_app/pubspec.yaml", pubspec, &[]);
        assert!(findings.is_empty());
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
