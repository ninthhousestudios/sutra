use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::constraints::DdEngine;
use crate::constraints::check::{self, EvalScope, FactsSource};
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
    use crate::constraints::constraint_coverage;

    let mut rules = rules::load_rules(workspace_root)?;
    let (all_constraints, constraint_parse_errors) = rules.all_constraints();
    let waivers = db.get_constraint_waivers(None)?;

    let mut waiver_counts: HashMap<&str, usize> = HashMap::new();
    for w in &waivers {
        *waiver_counts.entry(&w.constraint_id).or_default() += 1;
    }

    let all_files = db.all_files()?;
    let paths: Vec<&str> = all_files.iter().map(|f| &*f.path).collect();
    let comp_with_paths = db.active_components_with_paths()?;
    let component_names: Vec<&str> = comp_with_paths
        .iter()
        .map(|(_, name, _)| name.as_str())
        .collect();
    let component_ids: Vec<&str> = comp_with_paths
        .iter()
        .map(|(id, _, _)| id.as_str())
        .collect();

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
                ConstraintKind::ForbiddenExternal {
                    from,
                    crates,
                    include_dev,
                } => json!({ "from": from, "crates": crates, "include_dev": include_dev }),
                ConstraintKind::ConfinedExternal {
                    crates,
                    allowed_in,
                    include_dev,
                } => {
                    json!({ "crates": crates, "allowed_in": allowed_in, "include_dev": include_dev })
                }
                ConstraintKind::ForbiddenPattern {
                    language,
                    query,
                } => json!({ "language": language, "query": query }),
            };

            let coverage = constraint_coverage(c, &paths, &component_names, &component_ids);
            let coverage_fields: serde_json::Map<String, serde_json::Value> = coverage
                .fields
                .iter()
                .map(|(name, count)| (name.to_string(), json!(count)))
                .collect();
            let dead_fields = coverage.dead_fields();

            let mut entry = json!({
                "id": c.id,
                "kind": c.kind.kind_tag(),
                "kind_detail": kind_detail,
                "severity": c.severity.as_str(),
                "name": c.name,
                "provenance": c.provenance,
                "scope": c.scope,
                "waiver_count": waiver_counts.get(&*c.id).copied().unwrap_or(0),
                "matched_file_count": coverage_fields,
            });
            if !dead_fields.is_empty() {
                entry["warning"] = json!(format!(
                    "zero matches on {}: constraint is inert",
                    dead_fields.join(", "),
                ));
            }
            entry
        })
        .collect();

    let mut result = json!({ "constraints": constraints_out });
    if !constraint_parse_errors.is_empty() {
        result["parse_errors"] = json!(
            constraint_parse_errors
                .iter()
                .map(|e| json!({
                    "severity": "blocking",
                    "index": e.index,
                    "name": e.name,
                    "error": e.error,
                }))
                .collect::<Vec<_>>()
        );
    }
    Ok(result)
}

fn handle_violations(
    db: &Db,
    workspace_root: &Path,
    dd_engine: Option<&DdEngine>,
) -> Result<serde_json::Value> {
    let registry = crate::parser::adapter::default_registry();
    let outcome = check::evaluate(
        &FactsSource::DdBacked { db, dd_engine },
        workspace_root,
        EvalScope::Workspace,
        &registry,
    )?;

    let active: Vec<_> = outcome.active.iter().map(finding_to_json).collect();
    let waived: Vec<_> = outcome
        .waived
        .iter()
        .map(|w| {
            let mut v = finding_to_json(&w.finding);
            v["rationale"] = json!(w.rationale);
            v["waived_by"] = json!(w.waived_by);
            v
        })
        .collect();

    let mut result = json!({
        "violations": active,
        "waived_violations": waived,
    });
    if !outcome.parse_errors.is_empty() {
        result["parse_errors"] = json!(
            outcome
                .parse_errors
                .iter()
                .map(|e| json!({
                    "severity": "blocking",
                    "index": e.index,
                    "name": e.name,
                    "error": e.error,
                }))
                .collect::<Vec<_>>()
        );
    }
    Ok(result)
}

fn finding_to_json(f: &check::ConstraintFinding) -> serde_json::Value {
    let mut v = json!({
        "constraint_id": f.constraint_id,
        "constraint_name": f.constraint_name,
        "constraint_kind": f.constraint_kind,
        "severity": f.severity.as_str(),
        "provenance": f.provenance,
        "from_path": f.from_path,
        "to_path": f.to_path,
        "component_context": f.component_context,
        "detail": f.detail,
    });
    if let Some(line) = f.line {
        v["line"] = json!(line);
    }
    if let Some(snippet) = &f.snippet {
        v["snippet"] = json!(snippet);
    }
    if let Some(sym) = &f.enclosing_symbol {
        v["enclosing_symbol"] = json!(sym);
    }
    v
}

fn handle_waive(db: &Db, args: &ConstraintsArgs) -> Result<serde_json::Value> {
    let constraint_id = args
        .constraint_id
        .as_ref()
        .ok_or_else(|| SutraError::Internal("waive requires constraint_id".into()))?;
    let file_path = args
        .file_path
        .as_ref()
        .ok_or_else(|| SutraError::Internal("waive requires file_path".into()))?;
    let rationale = args
        .rationale
        .as_ref()
        .ok_or_else(|| SutraError::Internal("waive requires rationale".into()))?;
    let waived_by = args
        .waived_by
        .as_ref()
        .ok_or_else(|| SutraError::Internal("waive requires waived_by".into()))?;

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
    let waiver_id = args
        .waiver_id
        .ok_or_else(|| SutraError::Internal("unwaive requires waiver_id".into()))?;

    let deleted = db.delete_constraint_waiver(waiver_id)?;

    if !deleted {
        return Err(SutraError::Internal(format!(
            "waiver {waiver_id} not found"
        )));
    }

    Ok(json!({ "revoked": waiver_id }))
}
