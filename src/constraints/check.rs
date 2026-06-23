use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use glob::{MatchOptions, Pattern};

use crate::constraints::{self, ConstraintResolver, DdEngine, DdFacts, external};
pub use crate::constraints::{ConstraintFinding, FindingDelta};
use crate::error::Result;
use crate::rules::{
    self, Constraint, ConstraintKind, ConstraintParseError, Severity, match_no_cycles_constraint,
};
use crate::waivers::{self, Waived};

#[derive(Debug, Clone, Default)]
pub struct CheckOutcome {
    pub active: Vec<ConstraintFinding>,
    pub waived: Vec<Waived<ConstraintFinding>>,
    pub resolved: Vec<ConstraintFinding>,
    pub parse_errors: Vec<ConstraintParseError>,
}

pub enum EvalScope<'a> {
    Workspace,
    ChangedFiles {
        changed_ids: &'a HashSet<i64>,
        old_edges: &'a HashSet<(i64, i64)>,
    },
    SingleFile(i64),
    Edges {
        edges: &'a [(i64, i64)],
        /// Proposed `(from_path, crate_name)` external imports (guard side).
        externals: &'a [(String, String)],
    },
}

pub enum FactsSource<'a> {
    DdBacked {
        db: &'a crate::db::Db,
        dd_engine: Option<&'a DdEngine>,
    },
    RawConn(&'a rusqlite::Connection),
}

pub fn evaluate(
    facts: &FactsSource,
    workspace_root: &Path,
    scope: EvalScope,
) -> Result<CheckOutcome> {
    match facts {
        FactsSource::DdBacked { db, dd_engine } => {
            evaluate_dd(db, *dd_engine, workspace_root, scope)
        }
        FactsSource::RawConn(conn) => evaluate_raw(conn, workspace_root, scope),
    }
}

fn evaluate_dd(
    db: &crate::db::Db,
    dd_engine: Option<&DdEngine>,
    workspace_root: &Path,
    scope: EvalScope,
) -> Result<CheckOutcome> {
    let loaded_rules = rules::load_rules(workspace_root)?;
    let (all_constraints, parse_errors) = loaded_rules.all_constraints();

    let all_files = db.all_files()?;
    let path_map: HashMap<i64, &str> = all_files.iter().map(|f| (f.id, &*f.path)).collect();

    let comp_with_paths = db.active_components_with_paths()?;
    let mut file_to_component: HashMap<&str, &str> = HashMap::new();
    let mut comp_name_to_id: HashMap<&str, &str> = HashMap::new();
    for (comp_id, name, paths) in &comp_with_paths {
        comp_name_to_id.insert(name, comp_id);
        for path in paths {
            file_to_component.insert(path, comp_id);
        }
    }

    let changed_ids = match &scope {
        EvalScope::ChangedFiles { changed_ids, .. } => Some(*changed_ids),
        _ => None,
    };

    let mut findings = Vec::new();
    let mut resolved = Vec::new();

    // Dead constraint detection (Workspace and ChangedFiles scopes only)
    if !matches!(scope, EvalScope::SingleFile(_) | EvalScope::Edges { .. }) {
        let paths: Vec<&str> = all_files.iter().map(|f| &*f.path).collect();
        let component_names: Vec<&str> = comp_with_paths
            .iter()
            .map(|(_, name, _)| name.as_str())
            .collect();
        let component_ids: Vec<&str> = comp_with_paths
            .iter()
            .map(|(id, _, _)| id.as_str())
            .collect();
        for c in &all_constraints {
            let coverage =
                constraints::constraint_coverage(c, &paths, &component_names, &component_ids);
            let dead = coverage.dead_fields();
            if !dead.is_empty() {
                findings.push(ConstraintFinding {
                    constraint_id: c.id.clone(),
                    constraint_name: c.name.clone(),
                    constraint_kind: "dead_constraint".into(),
                    severity: Severity::Informational,
                    provenance: c.provenance.clone(),
                    from_path: String::new(),
                    to_path: String::new(),
                    component_context: None,
                    detail: format!(
                        "{} constraint '{}': zero matches on {} — rule is inert",
                        c.kind.kind_tag(),
                        c.name.as_deref().unwrap_or(&c.id),
                        dead.join(", "),
                    ),
                    delta: FindingDelta::Unknown,
                });
            }
        }
    }

    if external::has_external_constraints(&all_constraints) {
        let unresolved = db.unresolved_imports_with_files()?;
        let layout = crate::rust_imports::parse_workspace_layout(workspace_root);
        let crate_names = layout.all_crate_names();
        let crate_name_refs: Vec<&str> = crate_names.iter().copied().collect();
        if let Err(msg) =
            external::validate_no_external_targeting_members(&all_constraints, &crate_name_refs)
        {
            findings.push(external::config_error_finding(&msg));
        }
        findings.extend(external::check_workspace_externals(
            &all_constraints,
            workspace_root,
            &unresolved,
            changed_ids,
            &crate_name_refs,
        ));
    }

    let edges = db.import_edges()?;
    if edges.is_empty() {
        let constraint_waivers = db.get_constraint_waivers(None)?;
        let (active, waived) = waivers::partition(findings, &constraint_waivers);
        return Ok(CheckOutcome {
            active,
            waived,
            resolved,
            parse_errors,
        });
    }

    let ephemeral;
    let engine: &DdEngine = if let Some(e) = dd_engine {
        if e.is_invalidated() {
            e.reload(DdFacts {
                import_edges: edges.clone(),
            });
        } else if !e.is_loaded() {
            e.ingest(DdFacts {
                import_edges: edges.clone(),
            })?;
        }
        e
    } else {
        ephemeral = DdEngine::new(Duration::from_secs(60));
        ephemeral.ingest(DdFacts {
            import_edges: edges.clone(),
        })?;
        &ephemeral
    };

    let mut resolver = ConstraintResolver::new();
    let pairs = resolver.resolve(&all_constraints, db, &path_map)?;

    if !pairs.is_empty() {
        engine.set_forbidden_pairs(pairs)?;
        let current_violations = engine.query_violations()?;

        let (baseline_set, delta_available) = match &scope {
            EvalScope::ChangedFiles {
                old_edges,
                changed_ids,
            } => {
                let current_changed_edges: HashSet<(i64, i64)> = edges
                    .iter()
                    .filter(|(src, _)| changed_ids.contains(src))
                    .copied()
                    .collect();

                let new_edges: Vec<(i64, i64)> = current_changed_edges
                    .iter()
                    .copied()
                    .filter(|e| !old_edges.contains(e))
                    .collect();

                let removed_edges: Vec<(i64, i64)> = old_edges
                    .iter()
                    .copied()
                    .filter(|e| !current_changed_edges.contains(e))
                    .collect();

                let baseline = if !new_edges.is_empty() || !removed_edges.is_empty() {
                    engine.update(super::DdDelta {
                        added_edges: removed_edges.clone(),
                        removed_edges: new_edges.clone(),
                    })?;
                    let baseline_result = engine.query_violations();
                    engine.update(super::DdDelta {
                        added_edges: new_edges,
                        removed_edges,
                    })?;
                    baseline_result?.into_iter().collect()
                } else {
                    current_violations.iter().copied().collect()
                };
                (baseline, true)
            }
            _ => (HashSet::new(), false),
        };

        let current_set: HashSet<(i64, i64)> = current_violations.iter().copied().collect();

        for &(from_id, to_id) in &current_violations {
            if let Some(cids) = changed_ids
                && !cids.contains(&from_id)
                && !cids.contains(&to_id)
            {
                continue;
            }

            let from_path = path_map.get(&from_id).copied().unwrap_or("");
            let to_path = path_map.get(&to_id).copied().unwrap_or("");

            if let Some(c) = constraints::find_matching_constraint(
                &all_constraints,
                from_path,
                to_path,
                &file_to_component,
                &comp_name_to_id,
            ) {
                let delta = if delta_available {
                    if baseline_set.contains(&(from_id, to_id)) {
                        FindingDelta::PreExisting
                    } else {
                        FindingDelta::Introduced
                    }
                } else {
                    FindingDelta::Unknown
                };

                findings.push(make_finding(
                    c,
                    &from_path,
                    &to_path,
                    &file_to_component,
                    delta,
                ));
            }
        }

        if delta_available {
            for &(from_id, to_id) in &baseline_set {
                if current_set.contains(&(from_id, to_id)) {
                    continue;
                }
                if let Some(cids) = changed_ids
                    && !cids.contains(&from_id)
                    && !cids.contains(&to_id)
                {
                    continue;
                }
                let from_path = path_map.get(&from_id).copied().unwrap_or("");
                let to_path = path_map.get(&to_id).copied().unwrap_or("");
                if let Some(c) = constraints::find_matching_constraint(
                    &all_constraints,
                    from_path,
                    to_path,
                    &file_to_component,
                    &comp_name_to_id,
                ) {
                    resolved.push(make_finding(
                        c,
                        from_path,
                        to_path,
                        &file_to_component,
                        FindingDelta::Resolved,
                    ));
                }
            }
        }
    }

    // Cycle detection
    let cycle_filter = changed_ids;
    for cycle in engine.query_cycles()? {
        if let Some(cids) = cycle_filter
            && !cycle.file_ids.iter().any(|id| cids.contains(id))
        {
            continue;
        }
        let cycle_paths: Vec<&str> = cycle
            .file_ids
            .iter()
            .filter_map(|id| path_map.get(id).copied())
            .collect();
        let matched = match_no_cycles_constraint(&all_constraints, &cycle_paths);
        findings.push(ConstraintFinding {
            constraint_id: matched
                .map(|c| c.id.clone())
                .unwrap_or_else(|| "builtin:cycles".into()),
            constraint_name: matched.and_then(|c| c.name.clone()),
            constraint_kind: "no_cycles".into(),
            severity: matched.map(|c| c.severity).unwrap_or(Severity::Blocking),
            provenance: matched.and_then(|c| c.provenance.clone()),
            from_path: cycle_paths.first().unwrap_or(&"").to_string(),
            to_path: cycle_paths.last().unwrap_or(&"").to_string(),
            component_context: None,
            detail: format!("import cycle: {}", cycle_paths.join(" -> ")),
            delta: FindingDelta::Unknown,
        });
    }

    // MaxFanIn evaluation
    let glob_opts = MatchOptions {
        require_literal_separator: true,
        ..MatchOptions::default()
    };
    for c in &all_constraints {
        let ConstraintKind::MaxFanIn { target, threshold } = &c.kind else {
            continue;
        };
        let pat = match Pattern::new(target) {
            Ok(p) => p,
            Err(_) => continue,
        };
        for f in &all_files {
            if f.fan_in_files <= *threshold as i64 {
                continue;
            }
            if !pat.matches_with(&f.path, glob_opts) {
                continue;
            }
            findings.push(ConstraintFinding {
                constraint_id: c.id.clone(),
                constraint_name: c.name.clone(),
                constraint_kind: "max_fan_in".into(),
                severity: c.severity,
                provenance: c.provenance.clone(),
                from_path: f.path.to_string(),
                to_path: String::new(),
                component_context: None,
                detail: format!(
                    "fan-in is {}, threshold is {threshold}: {}",
                    f.fan_in_files, f.path,
                ),
                delta: FindingDelta::Unknown,
            });
        }
    }

    let constraint_waivers = db.get_constraint_waivers(None)?;
    let (active, waived) = waivers::partition(findings, &constraint_waivers);

    Ok(CheckOutcome {
        active,
        waived,
        resolved,
        parse_errors,
    })
}

fn evaluate_raw(
    conn: &rusqlite::Connection,
    workspace_root: &Path,
    scope: EvalScope,
) -> Result<CheckOutcome> {
    use rusqlite::params;

    let loaded_rules = rules::load_rules(workspace_root)?;
    let (all_constraints, parse_errors) = loaded_rules.all_constraints();

    let parse_error_findings: Vec<ConstraintFinding> = parse_errors
        .iter()
        .map(|e| ConstraintFinding {
            constraint_id: Arc::from(format!("parse-error-{}", e.index)),
            constraint_name: e.name.as_deref().map(Arc::from),
            constraint_kind: "parse_error".to_string(),
            severity: Severity::Blocking,
            provenance: None,
            from_path: String::new(),
            to_path: String::new(),
            component_context: None,
            detail: format!(
                "malformed [[constraint]] at index {}{}: {}",
                e.index,
                e.name
                    .as_deref()
                    .map(|n| format!(" (name: {n})"))
                    .unwrap_or_default(),
                e.error,
            ),
            delta: FindingDelta::Unknown,
        })
        .collect();

    let has_forbidden_or_boundary = all_constraints.iter().any(|c| {
        matches!(
            c.kind,
            rules::ConstraintKind::ForbiddenDep { .. } | rules::ConstraintKind::Boundary { .. }
        )
    });
    let has_external = external::has_external_constraints(&all_constraints);
    let has_max_fan_in = all_constraints
        .iter()
        .any(|c| matches!(c.kind, rules::ConstraintKind::MaxFanIn { .. }));
    if !has_forbidden_or_boundary && !has_external && !has_max_fan_in {
        return Ok(CheckOutcome {
            active: parse_error_findings,
            parse_errors,
            ..Default::default()
        });
    }

    let mut external_findings: Vec<ConstraintFinding> = Vec::new();
    if has_external {
        let layout = crate::rust_imports::parse_workspace_layout(workspace_root);
        let crate_names = layout.all_crate_names();
        let crate_name_refs: Vec<&str> = crate_names.iter().copied().collect();
        if let Err(msg) =
            external::validate_no_external_targeting_members(&all_constraints, &crate_name_refs)
        {
            external_findings.push(external::config_error_finding(&msg));
        }
        match &scope {
            EvalScope::SingleFile(file_id) => {
                let mut stmt = conn.prepare(
                    "SELECT f.path, f.language, i.imported_path FROM imports i \
                     JOIN files f ON f.id = i.file_id \
                     WHERE i.file_id = ?1 AND i.resolved_file_id IS NULL",
                )?;
                let rows: Vec<(String, String, String)> = stmt
                    .query_map(params![file_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                let items: Vec<(String, String)> = rows
                    .iter()
                    .filter_map(|(path, lang, imp)| {
                        external::external_crate_of_import(imp, lang, &crate_name_refs)
                            .map(|c| (path.clone(), c))
                    })
                    .collect();
                external_findings.extend(external::check_import_items(&all_constraints, &items));
            }
            EvalScope::Edges { externals, .. } => {
                external_findings.extend(external::check_import_items(&all_constraints, externals));
            }
            _ => {}
        }
    }

    let edges: Vec<(i64, i64)> = match &scope {
        EvalScope::SingleFile(file_id) => {
            let mut stmt = conn.prepare(
                "SELECT file_id, resolved_file_id FROM imports \
                 WHERE (file_id = ?1 OR resolved_file_id = ?1) \
                 AND resolved_file_id IS NOT NULL",
            )?;
            stmt.query_map(params![file_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .filter_map(|r| r.ok())
                .collect()
        }
        EvalScope::Edges { edges, .. } => edges.to_vec(),
        _ => {
            return Err(crate::error::SutraError::Internal(
                "RawConn only supports SingleFile and Edges scopes".into(),
            ));
        }
    };

    if edges.is_empty() && external_findings.is_empty() && !has_max_fan_in {
        return Ok(CheckOutcome {
            active: parse_error_findings,
            parse_errors,
            ..Default::default()
        });
    }

    // Build path map for referenced file IDs
    let mut needed_ids: Vec<i64> = edges.iter().flat_map(|(a, b)| [*a, *b]).collect();
    needed_ids.sort_unstable();
    needed_ids.dedup();

    let path_map: HashMap<i64, String> = if needed_ids.is_empty() {
        HashMap::new()
    } else {
        let placeholders: String = needed_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("SELECT id, path FROM files WHERE id IN ({placeholders})");
        let mut stmt = conn.prepare(&sql)?;
        stmt.query_map(rusqlite::params_from_iter(needed_ids.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect()
    };

    let comp_rows = build_component_rows_raw(conn)?;
    let mut file_to_component: HashMap<&str, &str> = HashMap::new();
    let mut comp_name_to_id: HashMap<&str, &str> = HashMap::new();
    for (comp_id, name, paths) in &comp_rows {
        comp_name_to_id.insert(name, comp_id);
        for path in paths {
            file_to_component.insert(path, comp_id);
        }
    }

    use crate::db::ConstraintWaiverRow;

    let relevant_paths: HashSet<&str> = path_map
        .values()
        .map(|p| p.as_str())
        .chain(external_findings.iter().map(|f| f.from_path.as_str()))
        .collect();
    let constraint_waivers: Vec<ConstraintWaiverRow> = conn
        .prepare(
            "SELECT id, constraint_id, constraint_name, file_path, \
             symbol_qualified_name, rationale, waived_by, created_at, updated_at \
             FROM constraint_waivers",
        )?
        .query_map([], |row| {
            Ok(ConstraintWaiverRow {
                id: row.get(0)?,
                constraint_id: row.get(1)?,
                constraint_name: row.get(2)?,
                file_path: row.get(3)?,
                symbol_qualified_name: row.get(4)?,
                rationale: row.get(5)?,
                waived_by: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        })?
        .filter_map(|r| r.ok())
        .filter(|w| relevant_paths.contains(w.file_path.as_str()))
        .collect();

    let mut findings = external_findings;
    for (from_id, to_id) in &edges {
        let from = match path_map.get(from_id) {
            Some(p) => p,
            None => continue,
        };
        let to = match path_map.get(to_id) {
            Some(p) => p,
            None => continue,
        };

        if let Some(c) = constraints::find_matching_constraint(
            &all_constraints,
            from,
            to,
            &file_to_component,
            &comp_name_to_id,
        ) {
            findings.push(make_finding(
                c,
                from,
                to,
                &file_to_component,
                FindingDelta::Unknown,
            ));
        }
    }

    // MaxFanIn evaluation
    if has_max_fan_in {
        let glob_opts = MatchOptions {
            require_literal_separator: true,
            ..MatchOptions::default()
        };
        let fan_in_targets: Vec<(String, i64)> = match &scope {
            EvalScope::SingleFile(file_id) => conn
                .prepare("SELECT path, fan_in_files FROM files WHERE id = ?1")?
                .query_row(params![file_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok()
                .into_iter()
                .collect(),
            EvalScope::Edges { edges, .. } => {
                let target_ids: HashSet<i64> = edges.iter().map(|(_, t)| *t).collect();
                let mut rows = Vec::new();
                let mut stmt =
                    conn.prepare("SELECT path, fan_in_files FROM files WHERE id = ?1")?;
                for tid in &target_ids {
                    if let Ok(row) = stmt.query_row(params![tid], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    }) {
                        rows.push(row);
                    }
                }
                rows
            }
            _ => Vec::new(),
        };
        for (path, fan_in) in &fan_in_targets {
            for c in &all_constraints {
                let ConstraintKind::MaxFanIn { target, threshold } = &c.kind else {
                    continue;
                };
                if *fan_in <= *threshold as i64 {
                    continue;
                }
                if let Ok(pat) = Pattern::new(target)
                    && pat.matches_with(path, glob_opts)
                {
                    findings.push(ConstraintFinding {
                        constraint_id: c.id.clone(),
                        constraint_name: c.name.clone(),
                        constraint_kind: "max_fan_in".into(),
                        severity: c.severity,
                        provenance: c.provenance.clone(),
                        from_path: path.clone(),
                        to_path: String::new(),
                        component_context: None,
                        detail: format!("fan-in is {fan_in}, threshold is {threshold}: {path}",),
                        delta: FindingDelta::Unknown,
                    });
                }
            }
        }
    }

    let (mut active, waived) = waivers::partition(findings, &constraint_waivers);

    let mut all_active = parse_error_findings;
    all_active.append(&mut active);

    Ok(CheckOutcome {
        active: all_active,
        waived,
        parse_errors,
        ..Default::default()
    })
}

/// Check a manifest's findings against waivers and return a partitioned outcome.
fn partition_manifest_findings(
    conn: &rusqlite::Connection,
    manifest_rel_path: &str,
    findings: Vec<ConstraintFinding>,
) -> Result<(
    Vec<ConstraintFinding>,
    Vec<waivers::Waived<ConstraintFinding>>,
)> {
    use crate::db::ConstraintWaiverRow;
    use rusqlite::params;

    if findings.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let constraint_waivers: Vec<ConstraintWaiverRow> = conn
        .prepare(
            "SELECT id, constraint_id, constraint_name, file_path, \
             symbol_qualified_name, rationale, waived_by, created_at, updated_at \
             FROM constraint_waivers WHERE file_path = ?1",
        )?
        .query_map(params![manifest_rel_path], |row| {
            Ok(ConstraintWaiverRow {
                id: row.get(0)?,
                constraint_id: row.get(1)?,
                constraint_name: row.get(2)?,
                file_path: row.get(3)?,
                symbol_qualified_name: row.get(4)?,
                rationale: row.get(5)?,
                waived_by: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(waivers::partition(findings, &constraint_waivers))
}

/// Check a single (possibly proposed) Cargo manifest against external-crate
/// constraints, with waiver partitioning. Used by the guard on Cargo.toml edits.
///
/// When checking the root Cargo.toml, also re-checks member manifests if the
/// proposed edit changes workspace dependency renames — a rename change can
/// cause `workspace = true` aliases in members to resolve to constrained packages.
pub fn check_manifest_raw(
    conn: &rusqlite::Connection,
    workspace_root: &Path,
    manifest_rel_path: &str,
    content: &str,
) -> Result<CheckOutcome> {
    let loaded_rules = rules::load_rules(workspace_root)?;
    let (all_constraints, parse_errors) = loaded_rules.all_constraints();
    if !external::has_external_constraints(&all_constraints) {
        return Ok(CheckOutcome {
            parse_errors,
            ..Default::default()
        });
    }

    let layout = crate::rust_imports::parse_workspace_layout(workspace_root);
    let crate_names = layout.all_crate_names();
    let crate_name_refs: Vec<&str> = crate_names.iter().copied().collect();

    let is_root = manifest_rel_path == "Cargo.toml";
    let ws_renames = if is_root {
        external::workspace_dep_renames(content)
    } else {
        std::fs::read_to_string(workspace_root.join("Cargo.toml"))
            .ok()
            .map(|c| external::workspace_dep_renames(&c))
            .unwrap_or_default()
    };
    let renames = if ws_renames.is_empty() {
        None
    } else {
        Some(&ws_renames)
    };
    let mut findings =
        external::check_manifest(&all_constraints, manifest_rel_path, content, renames);
    if let Err(msg) =
        external::validate_no_external_targeting_members(&all_constraints, &crate_name_refs)
    {
        findings.push(external::config_error_finding(&msg));
    }

    let (mut active, waived) = partition_manifest_findings(conn, manifest_rel_path, findings)?;

    // When the root manifest's rename map changed, re-check declared workspace
    // members with the proposed renames — a new/changed alias may resolve to a
    // constrained package in members that use `workspace = true`.
    // Only surfaces *newly introduced* findings (not pre-existing violations).
    if is_root && !ws_renames.is_empty() {
        let old_renames = std::fs::read_to_string(workspace_root.join("Cargo.toml"))
            .ok()
            .map(|c| external::workspace_dep_renames(&c))
            .unwrap_or_default();
        if ws_renames != old_renames {
            let old_rename_ref = if old_renames.is_empty() {
                None
            } else {
                Some(&old_renames)
            };
            for (member_rel, member_content) in
                external::workspace_member_manifests(workspace_root, content)
            {
                let old_findings = external::check_manifest(
                    &all_constraints,
                    &member_rel,
                    &member_content,
                    old_rename_ref,
                );
                let old_keys: std::collections::HashSet<(&str, &str)> = old_findings
                    .iter()
                    .map(|f| (&*f.constraint_id, f.to_path.as_str()))
                    .collect();
                let new_findings: Vec<_> = external::check_manifest(
                    &all_constraints,
                    &member_rel,
                    &member_content,
                    renames,
                )
                .into_iter()
                .filter(|f| !old_keys.contains(&(&*f.constraint_id, f.to_path.as_str())))
                .collect();
                let (member_active, _) =
                    partition_manifest_findings(conn, &member_rel, new_findings)?;
                active.extend(member_active);
            }
        }
    }

    Ok(CheckOutcome {
        active,
        waived,
        parse_errors,
        ..Default::default()
    })
}

/// Check a single pubspec.yaml against external-crate constraints,
/// with waiver partitioning. Used by the guard on pubspec.yaml edits.
pub fn check_pubspec_raw(
    conn: &rusqlite::Connection,
    workspace_root: &Path,
    pubspec_rel_path: &str,
    content: &str,
) -> Result<CheckOutcome> {
    use rusqlite::params;

    let loaded_rules = rules::load_rules(workspace_root)?;
    let (all_constraints, parse_errors) = loaded_rules.all_constraints();
    if !external::has_external_constraints(&all_constraints) {
        return Ok(CheckOutcome {
            parse_errors,
            ..Default::default()
        });
    }

    let findings = external::check_pubspec(&all_constraints, pubspec_rel_path, content);
    if findings.is_empty() {
        return Ok(CheckOutcome {
            parse_errors,
            ..Default::default()
        });
    }

    use crate::db::ConstraintWaiverRow;
    let constraint_waivers: Vec<ConstraintWaiverRow> = conn
        .prepare(
            "SELECT id, constraint_id, constraint_name, file_path, \
             symbol_qualified_name, rationale, waived_by, created_at, updated_at \
             FROM constraint_waivers WHERE file_path = ?1",
        )?
        .query_map(params![pubspec_rel_path], |row| {
            Ok(ConstraintWaiverRow {
                id: row.get(0)?,
                constraint_id: row.get(1)?,
                constraint_name: row.get(2)?,
                file_path: row.get(3)?,
                symbol_qualified_name: row.get(4)?,
                rationale: row.get(5)?,
                waived_by: row.get(6)?,
                created_at: row.get(7)?,
                updated_at: row.get(8)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    let (active, waived) = waivers::partition(findings, &constraint_waivers);
    Ok(CheckOutcome {
        active,
        waived,
        parse_errors,
        ..Default::default()
    })
}

fn build_component_rows_raw(
    conn: &rusqlite::Connection,
) -> Result<Vec<(String, String, Vec<String>)>> {
    let mut out = Vec::new();
    let mut stmt =
        conn.prepare("SELECT id, name, prior_paths FROM components WHERE dissolved_at IS NULL")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (id, name, json) = row?;
        let paths = json
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();
        out.push((id, name, paths));
    }
    Ok(out)
}

fn make_finding(
    c: &Constraint,
    from_path: &str,
    to_path: &str,
    file_to_component: &HashMap<&str, &str>,
    delta: FindingDelta,
) -> ConstraintFinding {
    ConstraintFinding {
        constraint_id: c.id.clone(),
        constraint_name: c.name.clone(),
        constraint_kind: c.kind.kind_tag().to_string(),
        severity: c.severity,
        provenance: c.provenance.clone(),
        from_path: from_path.to_string(),
        to_path: to_path.to_string(),
        component_context: constraints::build_component_context(
            &c.kind,
            file_to_component,
            from_path,
            to_path,
        ),
        detail: constraints::format_violation_detail(
            c,
            from_path,
            to_path,
            delta == FindingDelta::Introduced,
        ),
        delta,
    }
}
