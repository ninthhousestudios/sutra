use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use crate::constraints::{self, ConstraintResolver, DdEngine, DdFacts};
use crate::error::Result;
use crate::rules::{self, Constraint, ConstraintParseError, Severity, match_no_cycles_constraint};

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

#[derive(Debug, Clone)]
pub struct WaivedFinding {
    pub finding: ConstraintFinding,
    pub rationale: String,
    pub waived_by: String,
}

#[derive(Debug, Clone, Default)]
pub struct CheckOutcome {
    pub active: Vec<ConstraintFinding>,
    pub waived: Vec<WaivedFinding>,
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
    Edges(&'a [(i64, i64)]),
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
        FactsSource::RawConn(_conn) => {
            todo!("RawConn path — commit 4")
        }
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

    let edges = db.import_edges()?;
    if edges.is_empty() {
        return Ok(CheckOutcome {
            parse_errors,
            ..Default::default()
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

    let changed_ids = match &scope {
        EvalScope::ChangedFiles { changed_ids, .. } => Some(*changed_ids),
        _ => None,
    };

    let mut findings = Vec::new();
    let mut resolved = Vec::new();

    if !pairs.is_empty() {
        engine.set_forbidden_pairs(pairs)?;
        let current_violations = engine.query_violations()?;

        let (baseline_set, delta_available) = match &scope {
            EvalScope::ChangedFiles {
                old_edges,
                changed_ids,
            } => {
                let new_edges: Vec<(i64, i64)> = edges
                    .iter()
                    .filter(|(src, _)| changed_ids.contains(src))
                    .copied()
                    .filter(|e| !old_edges.contains(e))
                    .collect();

                let baseline = if !new_edges.is_empty() {
                    engine.update(super::DdDelta {
                        added_edges: vec![],
                        removed_edges: new_edges.clone(),
                    })?;
                    let baseline_result = engine.query_violations();
                    engine.update(super::DdDelta {
                        added_edges: new_edges,
                        removed_edges: vec![],
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
            if let Some(cids) = changed_ids {
                if !cids.contains(&from_id) && !cids.contains(&to_id) {
                    continue;
                }
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
                if let Some(cids) = changed_ids {
                    if !cids.contains(&from_id) && !cids.contains(&to_id) {
                        continue;
                    }
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
        if let Some(cids) = cycle_filter {
            if !cycle.file_ids.iter().any(|id| cids.contains(id)) {
                continue;
            }
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

    // Waiver partition — canonical rule: from_path only
    let waivers = db.get_constraint_waivers(None)?;
    let mut active = Vec::new();
    let mut waived = Vec::new();
    for f in findings {
        let waiver = waivers.iter().find(|w| {
            w.constraint_id == f.constraint_id
                && w.file_path == f.from_path
                && w.symbol_qualified_name.is_none()
        });
        if let Some(w) = waiver {
            waived.push(WaivedFinding {
                finding: f,
                rationale: w.rationale.clone(),
                waived_by: w.waived_by.clone(),
            });
        } else {
            active.push(f);
        }
    }

    Ok(CheckOutcome {
        active,
        waived,
        resolved,
        parse_errors,
    })
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
