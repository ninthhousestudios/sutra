use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use glob::{MatchOptions, Pattern};

use crate::constraints::{self, ConstraintResolver, DdEngine, DdFacts, external};
use crate::error::Result;
use crate::rules::{
    self, Constraint, ConstraintKind, ConstraintParseError, Severity, match_no_cycles_constraint,
};
use crate::waivers::{self, Waived};

#[derive(Debug, Clone)]
pub struct ConstraintFinding {
    pub constraint_id: String,
    pub constraint_name: Option<String>,
    pub constraint_kind: String,
    pub severity: Severity,
    pub provenance: Option<String>,
    pub from_path: String,
    pub to_path: String,
    pub component_context: Option<String>,
    pub detail: String,
    pub delta: FindingDelta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingDelta {
    Unknown,
    PreExisting,
    Introduced,
    Resolved,
}

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
    let path_map: HashMap<i64, String> = all_files.iter().map(|f| (f.id, f.path.clone())).collect();

    let comp_with_paths = db.active_components_with_paths()?;
    let mut file_to_component: HashMap<String, String> = HashMap::new();
    let mut comp_name_to_id: HashMap<String, String> = HashMap::new();
    for (comp_id, name, paths) in &comp_with_paths {
        comp_name_to_id.insert(name.clone(), comp_id.clone());
        for path in paths {
            file_to_component.insert(path.clone(), comp_id.clone());
        }
    }

    let changed_ids = match &scope {
        EvalScope::ChangedFiles { changed_ids, .. } => Some(*changed_ids),
        _ => None,
    };

    let mut findings = Vec::new();
    let mut resolved = Vec::new();

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

            let from_path = path_map.get(&from_id).cloned().unwrap_or_default();
            let to_path = path_map.get(&to_id).cloned().unwrap_or_default();

            if let Some(c) = constraints::find_matching_constraint(
                &all_constraints,
                &from_path,
                &to_path,
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
                let from_path = path_map.get(&from_id).cloned().unwrap_or_default();
                let to_path = path_map.get(&to_id).cloned().unwrap_or_default();
                if let Some(c) = constraints::find_matching_constraint(
                    &all_constraints,
                    &from_path,
                    &to_path,
                    &file_to_component,
                    &comp_name_to_id,
                ) {
                    resolved.push(make_finding(
                        c,
                        &from_path,
                        &to_path,
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
        let cycle_paths: Vec<String> = cycle
            .file_ids
            .iter()
            .filter_map(|id| path_map.get(id).cloned())
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
            from_path: cycle_paths.first().cloned().unwrap_or_default(),
            to_path: cycle_paths.last().cloned().unwrap_or_default(),
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
            if let Some(cids) = changed_ids
                && !cids.contains(&f.id)
            {
                continue;
            }
            findings.push(ConstraintFinding {
                constraint_id: c.id.clone(),
                constraint_name: c.name.clone(),
                constraint_kind: "max_fan_in".into(),
                severity: c.severity,
                provenance: c.provenance.clone(),
                from_path: f.path.clone(),
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
            constraint_id: format!("parse-error-{}", e.index),
            constraint_name: e.name.clone(),
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

    let (file_to_component, comp_name_to_id) = build_component_maps_raw(conn)?;

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

    // MaxFanIn evaluation (SingleFile scope only)
    if has_max_fan_in {
        if let EvalScope::SingleFile(file_id) = &scope {
            let fan_in_row: Option<(String, i64)> = conn
                .prepare("SELECT path, fan_in_files FROM files WHERE id = ?1")?
                .query_row(params![file_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok();
            if let Some((path, fan_in)) = fan_in_row {
                let glob_opts = MatchOptions {
                    require_literal_separator: true,
                    ..MatchOptions::default()
                };
                for c in &all_constraints {
                    let ConstraintKind::MaxFanIn { target, threshold } = &c.kind else {
                        continue;
                    };
                    if fan_in <= *threshold as i64 {
                        continue;
                    }
                    if let Ok(pat) = Pattern::new(target)
                        && pat.matches_with(&path, glob_opts)
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
                            detail: format!(
                                "fan-in is {fan_in}, threshold is {threshold}: {path}",
                            ),
                            delta: FindingDelta::Unknown,
                        });
                    }
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

/// Check a single (possibly proposed) Cargo manifest against external-crate
/// constraints, with waiver partitioning. Used by the guard on Cargo.toml edits.
pub fn check_manifest_raw(
    conn: &rusqlite::Connection,
    workspace_root: &Path,
    manifest_rel_path: &str,
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

    let layout = crate::rust_imports::parse_workspace_layout(workspace_root);
    let crate_names = layout.all_crate_names();
    let crate_name_refs: Vec<&str> = crate_names.iter().copied().collect();

    let mut findings = external::check_manifest(&all_constraints, manifest_rel_path, content);
    if let Err(msg) =
        external::validate_no_external_targeting_members(&all_constraints, &crate_name_refs)
    {
        findings.push(external::config_error_finding(&msg));
    }
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

    let (active, waived) = waivers::partition(findings, &constraint_waivers);
    Ok(CheckOutcome {
        active,
        waived,
        parse_errors,
        ..Default::default()
    })
}

fn build_component_maps_raw(
    conn: &rusqlite::Connection,
) -> Result<(HashMap<String, String>, HashMap<String, String>)> {
    let mut file_to_component: HashMap<String, String> = HashMap::new();
    let mut comp_name_to_id: HashMap<String, String> = HashMap::new();

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
        comp_name_to_id.insert(name, id.clone());
        if let Some(s) = json
            && let Ok(paths) = serde_json::from_str::<Vec<String>>(&s)
        {
            for path in paths {
                file_to_component.insert(path, id.clone());
            }
        }
    }

    Ok((file_to_component, comp_name_to_id))
}

fn make_finding(
    c: &Constraint,
    from_path: &str,
    to_path: &str,
    file_to_component: &HashMap<String, String>,
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
