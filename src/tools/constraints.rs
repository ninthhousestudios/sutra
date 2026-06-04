use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::constraints::{self, ConstraintResolver, DdEngine, DdFacts};
use crate::db::Db;
use crate::error::{Result, SutraError};
use crate::rules::{self, ConstraintKind};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConstraintsArgs {
    pub workspace: String,
    /// Action: "list", "violations", "waive", "unwaive"
    pub action: String,
    /// Constraint ID — 8-char blake3 hash (for waive)
    #[serde(default)]
    pub constraint_id: Option<String>,
    /// Human-readable constraint name (optional, stored with waiver for display)
    #[serde(default)]
    pub constraint_name: Option<String>,
    /// File path the waiver applies to (for waive)
    #[serde(default)]
    pub file_path: Option<String>,
    /// Symbol qualified name to scope the waiver (optional, for waive)
    #[serde(default)]
    pub symbol_qualified_name: Option<String>,
    /// Rationale for the waiver (for waive)
    #[serde(default)]
    pub rationale: Option<String>,
    /// Who is granting the waiver (for waive)
    #[serde(default)]
    pub waived_by: Option<String>,
    /// Waiver ID (for unwaive)
    #[serde(default)]
    pub waiver_id: Option<i64>,
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    dd_engine: Option<&DdEngine>,
    args: &ConstraintsArgs,
) -> Result<serde_json::Value> {
    match args.action.as_str() {
        "list" => handle_list(db, workspace_root),
        "violations" => handle_violations(db, workspace_root, dd_engine),
        "waive" => handle_waive(db, args),
        "unwaive" => handle_unwaive(db, args),
        other => Err(SutraError::Internal(format!(
            "unknown action: {other}. expected: list, violations, waive, unwaive"
        ))),
    }
}

fn handle_list(db: &Db, workspace_root: &Path) -> Result<serde_json::Value> {
    let rules = rules::load_rules(workspace_root)?;
    let all_constraints = rules.all_constraints()?;
    let waivers = db.get_constraint_waivers(None)?;

    let mut waiver_counts: HashMap<&str, usize> = HashMap::new();
    for w in &waivers {
        *waiver_counts.entry(&w.constraint_id).or_default() += 1;
    }

    let constraints_out: Vec<_> = all_constraints
        .iter()
        .map(|c| {
            let kind_detail = match &c.kind {
                ConstraintKind::ForbiddenDep { from, to } => {
                    json!({ "from": from, "to": to })
                }
                ConstraintKind::Boundary {
                    from_component,
                    to_component,
                } => json!({ "from_component": from_component, "to_component": to_component }),
                ConstraintKind::MaxFanIn { target, threshold } => {
                    json!({ "target": target, "threshold": threshold })
                }
                ConstraintKind::NoCycles => json!({}),
            };
            json!({
                "id": c.id,
                "kind": c.kind.kind_tag(),
                "kind_detail": kind_detail,
                "severity": c.severity.as_str(),
                "name": c.name,
                "provenance": c.provenance,
                "scope": c.scope,
                "waiver_count": waiver_counts.get(c.id.as_str()).copied().unwrap_or(0),
            })
        })
        .collect();

    Ok(json!({ "constraints": constraints_out }))
}

fn handle_violations(
    db: &Db,
    workspace_root: &Path,
    dd_engine: Option<&DdEngine>,
) -> Result<serde_json::Value> {
    let rules = rules::load_rules(workspace_root)?;
    let all_constraints = rules.all_constraints()?;
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
        return Ok(json!({
            "violations": [],
            "waived_violations": [],
            "note": "no import edges in workspace"
        }));
    }

    let ephemeral;
    let engine: &DdEngine = if let Some(e) = dd_engine.filter(|e| !e.is_invalidated()) {
        if !e.is_loaded() {
            e.ingest(DdFacts {
                import_edges: edges.clone(),
            })?;
        }
        e
    } else {
        ephemeral = DdEngine::new(Duration::from_secs(60));
        ephemeral.ingest(DdFacts {
            import_edges: edges,
        })?;
        &ephemeral
    };

    let mut resolver = ConstraintResolver::new();
    let pairs = resolver.resolve(&all_constraints, db, &path_map)?;

    let mut violation_list = Vec::new();

    if !pairs.is_empty() {
        engine.set_forbidden_pairs(pairs)?;
        let raw_violations = engine.query_violations()?;

        for &(from_id, to_id) in &raw_violations {
            let from_path = path_map.get(&from_id).cloned().unwrap_or_default();
            let to_path = path_map.get(&to_id).cloned().unwrap_or_default();
            if let Some(c) = constraints::find_matching_constraint(
                &all_constraints,
                &from_path,
                &to_path,
                &file_to_component,
                &comp_name_to_id,
            ) {
                let component_context = constraints::build_component_context(
                    &c.kind,
                    &file_to_component,
                    &from_path,
                    &to_path,
                );
                let detail =
                    constraints::format_violation_detail(c, &from_path, &to_path, false);
                violation_list.push(ViolationEntry {
                    constraint_id: c.id.clone(),
                    constraint_name: c.name.clone(),
                    constraint_kind: c.kind.kind_tag().to_string(),
                    severity: c.severity.as_str().to_string(),
                    provenance: c.provenance.clone(),
                    from_path,
                    to_path,
                    component_context,
                    detail,
                });
            }
        }
    }

    let no_cycles_constraint = all_constraints
        .iter()
        .find(|c| matches!(c.kind, ConstraintKind::NoCycles));

    for cycle in engine.query_cycles()? {
        let cycle_paths: Vec<String> = cycle
            .file_ids
            .iter()
            .filter_map(|id| path_map.get(id).cloned())
            .collect();
        violation_list.push(ViolationEntry {
            constraint_id: no_cycles_constraint
                .map(|c| c.id.clone())
                .unwrap_or_else(|| "builtin:cycles".into()),
            constraint_name: no_cycles_constraint.and_then(|c| c.name.clone()),
            constraint_kind: "no_cycles".into(),
            severity: no_cycles_constraint
                .map(|c| c.severity.as_str().to_string())
                .unwrap_or_else(|| "blocking".into()),
            provenance: no_cycles_constraint.and_then(|c| c.provenance.clone()),
            from_path: cycle_paths.first().cloned().unwrap_or_default(),
            to_path: cycle_paths.last().cloned().unwrap_or_default(),
            component_context: None,
            detail: format!("import cycle: {}", cycle_paths.join(" -> ")),
        });
    }

    // Partition into active vs waived
    let waivers = db.get_constraint_waivers(None)?;
    let mut waived = Vec::new();
    let mut active = Vec::new();
    for v in violation_list {
        let waiver = waivers.iter().find(|w| {
            w.constraint_id == v.constraint_id
                && (w.file_path == v.from_path || w.file_path == v.to_path)
        });
        if let Some(w) = waiver {
            waived.push(json!({
                "constraint_id": v.constraint_id,
                "constraint_name": v.constraint_name,
                "constraint_kind": v.constraint_kind,
                "severity": v.severity,
                "provenance": v.provenance,
                "from_path": v.from_path,
                "to_path": v.to_path,
                "component_context": v.component_context,
                "detail": v.detail,
                "rationale": w.rationale,
                "waived_by": w.waived_by,
            }));
        } else {
            active.push(json!({
                "constraint_id": v.constraint_id,
                "constraint_name": v.constraint_name,
                "constraint_kind": v.constraint_kind,
                "severity": v.severity,
                "provenance": v.provenance,
                "from_path": v.from_path,
                "to_path": v.to_path,
                "component_context": v.component_context,
                "detail": v.detail,
            }));
        }
    }

    Ok(json!({
        "violations": active,
        "waived_violations": waived,
    }))
}

struct ViolationEntry {
    constraint_id: String,
    constraint_name: Option<String>,
    constraint_kind: String,
    severity: String,
    provenance: Option<String>,
    from_path: String,
    to_path: String,
    component_context: Option<String>,
    detail: String,
}

fn handle_waive(db: &Db, args: &ConstraintsArgs) -> Result<serde_json::Value> {
    let constraint_id = args.constraint_id.as_ref().ok_or_else(|| {
        SutraError::Internal("waive requires constraint_id".into())
    })?;
    let file_path = args.file_path.as_ref().ok_or_else(|| {
        SutraError::Internal("waive requires file_path".into())
    })?;
    let rationale = args.rationale.as_ref().ok_or_else(|| {
        SutraError::Internal("waive requires rationale".into())
    })?;
    let waived_by = args.waived_by.as_ref().ok_or_else(|| {
        SutraError::Internal("waive requires waived_by".into())
    })?;

    let id = db.create_constraint_waiver(
        constraint_id,
        args.constraint_name.as_deref(),
        file_path,
        args.symbol_qualified_name.as_deref(),
        rationale,
        waived_by,
    )?;

    Ok(json!({
        "waiver_id": id,
        "constraint_id": constraint_id,
        "file_path": file_path,
    }))
}

fn handle_unwaive(db: &Db, args: &ConstraintsArgs) -> Result<serde_json::Value> {
    let waiver_id = args.waiver_id.ok_or_else(|| {
        SutraError::Internal("unwaive requires waiver_id".into())
    })?;

    let deleted = db.delete_constraint_waiver(waiver_id)?;

    if !deleted {
        return Err(SutraError::Internal(format!(
            "waiver {waiver_id} not found"
        )));
    }

    Ok(json!({ "revoked": waiver_id }))
}
