use std::collections::{HashMap, HashSet};
use std::path::Path;

use glob::{MatchOptions, Pattern};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::constraints::{self, ConstraintResolver, DdEngine, DdFacts};
use crate::conventions;
use crate::db::Db;
use crate::error::Result;
use crate::rules::{self, Constraint, ConstraintKind};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OrientArgs {
    pub workspace: String,
    /// Component name, component ID, or file path to orient on.
    pub scope: String,
}

struct ResolvedComponent {
    id: String,
    name: String,
    lifecycle_state: String,
    files: Vec<String>,
}

fn resolve_scope(db: &Db, scope: &str) -> Result<Vec<ResolvedComponent>> {
    let components = db.active_components_with_paths()?;
    let scope_lower = scope.to_lowercase();
    let mut results = Vec::new();

    for (comp_id, comp_name, paths) in &components {
        if comp_name.to_lowercase() == scope_lower || comp_id == scope {
            let lifecycle = db.component_lifecycle_state(comp_id).unwrap_or_else(|_| "stable".into());
            results.push(ResolvedComponent {
                id: comp_id.clone(),
                name: comp_name.clone(),
                lifecycle_state: lifecycle,
                files: paths.clone(),
            });
            return Ok(results);
        }
    }

    if let Ok(Some(alias)) = db.find_alias(scope) {
        if alias.target_kind == "component" {
            let lifecycle = db.component_lifecycle_state(&alias.target_ref).unwrap_or_else(|_| "stable".into());
            let files = components
                .iter()
                .find(|(id, _, _)| id == &alias.target_ref)
                .map(|(_, _, p)| p.clone())
                .unwrap_or_default();
            results.push(ResolvedComponent {
                id: alias.target_ref.clone(),
                name: alias.term,
                lifecycle_state: lifecycle,
                files,
            });
            return Ok(results);
        }
    }

    for (comp_id, comp_name, paths) in &components {
        if paths.iter().any(|p| p == scope) {
            let lifecycle = db.component_lifecycle_state(comp_id).unwrap_or_else(|_| "stable".into());
            results.push(ResolvedComponent {
                id: comp_id.clone(),
                name: comp_name.clone(),
                lifecycle_state: lifecycle,
                files: paths.clone(),
            });
            return Ok(results);
        }
    }

    let scope_prefix = if scope.ends_with('/') {
        scope.to_string()
    } else {
        format!("{scope}/")
    };
    for (comp_id, comp_name, paths) in &components {
        if paths.iter().any(|p| p.starts_with(&scope_prefix)) {
            let lifecycle = db.component_lifecycle_state(comp_id).unwrap_or_else(|_| "stable".into());
            results.push(ResolvedComponent {
                id: comp_id.clone(),
                name: comp_name.clone(),
                lifecycle_state: lifecycle,
                files: paths.clone(),
            });
        }
    }

    Ok(results)
}

fn check_drift_from_snapshots(
    db: &Db,
    component_id: &str,
    component_name: &str,
) -> Option<serde_json::Value> {
    let snapshots = db.recent_convention_snapshots(component_id, conventions::DRIFT_WINDOW).ok()?;
    if snapshots.len() < conventions::DRIFT_WINDOW {
        return None;
    }

    let newest = &snapshots[0];
    let oldest = &snapshots[conventions::DRIFT_WINDOW - 1];
    let delta = newest.entropy - oldest.entropy;

    if delta <= conventions::DRIFT_THRESHOLD {
        return None;
    }

    let entropies: Vec<f64> = snapshots.iter().rev().map(|s| s.entropy).collect();
    let monotonic = entropies.windows(2).all(|w| w[1] >= w[0]);
    if !monotonic {
        return None;
    }

    let old_dist: HashMap<String, f64> =
        serde_json::from_str(&oldest.attribute_distribution).unwrap_or_default();
    let new_dist: HashMap<String, f64> =
        serde_json::from_str(&newest.attribute_distribution).unwrap_or_default();
    let diverging = conventions::find_diverging_attributes(&old_dist, &new_dist);

    let diverging_out: Vec<_> = diverging
        .iter()
        .map(|d| {
            json!({
                "attribute": d.attribute,
                "old_proportion": d.old_proportion,
                "new_proportion": d.new_proportion,
            })
        })
        .collect();

    Some(json!({
        "component_id": component_id,
        "component_name": component_name,
        "entropy_old": oldest.entropy,
        "entropy_new": newest.entropy,
        "delta": delta,
        "diverging_attributes": diverging_out,
    }))
}

struct OrientViolation {
    constraint_id: String,
    constraint_name: Option<String>,
    severity: String,
    from_path: String,
    to_path: String,
    detail: String,
}

fn compute_violations(
    dd_engine: Option<&DdEngine>,
    db: &Db,
    all_constraints: &[Constraint],
    path_map: &HashMap<i64, String>,
    file_to_component: &HashMap<String, String>,
    comp_name_to_id: &HashMap<String, String>,
) -> Option<Vec<OrientViolation>> {
    let dd = dd_engine?;
    let edges = db.import_edges().ok()?;
    if edges.is_empty() {
        return Some(Vec::new());
    }

    if !dd.is_loaded() {
        dd.ingest(DdFacts {
            import_edges: edges,
        })
        .ok()?;
    }

    let mut resolver = ConstraintResolver::new();
    let pairs = resolver.resolve(all_constraints, db, path_map).ok()?;

    if !pairs.is_empty() {
        dd.set_forbidden_pairs(pairs).ok()?;
    }

    let raw_violations = dd.query_violations().ok()?;
    let mut result = Vec::new();

    for &(from_id, to_id) in &raw_violations {
        let from_path = path_map.get(&from_id).cloned().unwrap_or_default();
        let to_path = path_map.get(&to_id).cloned().unwrap_or_default();
        if let Some(c) = constraints::find_matching_constraint(
            all_constraints,
            &from_path,
            &to_path,
            file_to_component,
            comp_name_to_id,
        ) {
            let detail =
                constraints::format_violation_detail(c, &from_path, &to_path, false);
            result.push(OrientViolation {
                constraint_id: c.id.clone(),
                constraint_name: c.name.clone(),
                severity: format!("{:?}", c.severity).to_lowercase(),
                from_path,
                to_path,
                detail,
            });
        }
    }

    let no_cycles = all_constraints
        .iter()
        .find(|c| matches!(c.kind, ConstraintKind::NoCycles));
    if let Ok(cycles) = dd.query_cycles() {
        for cycle in cycles {
            let cycle_paths: Vec<String> = cycle
                .file_ids
                .iter()
                .filter_map(|id| path_map.get(id).cloned())
                .collect();
            result.push(OrientViolation {
                constraint_id: no_cycles
                    .map(|c| c.id.clone())
                    .unwrap_or_else(|| "builtin:cycles".into()),
                constraint_name: no_cycles.and_then(|c| c.name.clone()),
                severity: no_cycles
                    .map(|c| format!("{:?}", c.severity).to_lowercase())
                    .unwrap_or_else(|| "blocking".into()),
                from_path: cycle_paths.first().cloned().unwrap_or_default(),
                to_path: cycle_paths.last().cloned().unwrap_or_default(),
                detail: format!("import cycle: {}", cycle_paths.join(" -> ")),
            });
        }
    }

    Some(result)
}

fn constraints_for_component<'a>(
    all_constraints: &'a [Constraint],
    component_files: &[String],
    component_id: &str,
    comp_name_to_id: &HashMap<String, String>,
) -> Vec<&'a Constraint> {
    let opts = MatchOptions {
        require_literal_separator: true,
        ..MatchOptions::default()
    };
    all_constraints
        .iter()
        .filter(|c| {
            if let Some(scope) = &c.scope {
                let prefix = if scope.ends_with('/') {
                    scope.clone()
                } else {
                    format!("{scope}/")
                };
                let has_file = component_files
                    .iter()
                    .any(|f| f.starts_with(&prefix) || f == scope.as_str());
                if !has_file {
                    return false;
                }
                return true;
            }

            match &c.kind {
                ConstraintKind::ForbiddenDep { from, to } => {
                    let from_pat = Pattern::new(from).ok();
                    let to_pat = Pattern::new(to).ok();
                    component_files.iter().any(|f| {
                        from_pat
                            .as_ref()
                            .map_or(false, |p| p.matches_with(f, opts))
                            || to_pat
                                .as_ref()
                                .map_or(false, |p| p.matches_with(f, opts))
                    })
                }
                ConstraintKind::Boundary {
                    from_component,
                    to_component,
                } => {
                    from_component == component_id
                        || to_component == component_id
                        || comp_name_to_id
                            .get(from_component.as_str())
                            .map_or(false, |id| id == component_id)
                        || comp_name_to_id
                            .get(to_component.as_str())
                            .map_or(false, |id| id == component_id)
                }
                ConstraintKind::MaxFanIn { target, .. } => {
                    component_files.iter().any(|f| f == target)
                }
                ConstraintKind::NoCycles => true,
            }
        })
        .collect()
}

fn constraint_detail(c: &Constraint) -> String {
    match &c.kind {
        ConstraintKind::ForbiddenDep { from, to } => format!("{from} -> {to}"),
        ConstraintKind::Boundary {
            from_component,
            to_component,
        } => format!("{from_component} -> {to_component}"),
        ConstraintKind::MaxFanIn { target, threshold } => {
            format!("{target} (max {threshold})")
        }
        ConstraintKind::NoCycles => "no import cycles".into(),
    }
}

pub fn handle(
    db: &Db,
    scope: &str,
    workspace_root: &Path,
    dd_engine: Option<&DdEngine>,
) -> Result<serde_json::Value> {
    let components = resolve_scope(db, scope)?;
    if components.is_empty() {
        return Ok(json!({
            "scope": scope,
            "error": "no component found matching scope",
            "hint": "use sutra_components to list available components",
        }));
    }

    let all_conventions = db.all_conventions_merged()?;
    let all_proposals = db.pending_proposals()?;
    let all_waivers = db.list_waivers(None).unwrap_or_default();

    // Constraint system setup
    let loaded_rules = rules::load_rules(workspace_root)?;
    let all_constraints = loaded_rules.all_constraints().unwrap_or_default();
    let all_constraint_waivers = db.get_constraint_waivers(None).unwrap_or_default();

    let comp_with_paths = db.active_components_with_paths()?;
    let mut file_to_component: HashMap<String, String> = HashMap::new();
    let mut comp_name_to_id: HashMap<String, String> = HashMap::new();
    for (comp_id, name, paths) in &comp_with_paths {
        comp_name_to_id.insert(name.clone(), comp_id.clone());
        for path in paths {
            file_to_component.insert(path.clone(), comp_id.clone());
        }
    }

    // DD engine: ingest if needed, resolve constraint pairs
    let all_files = db.all_files()?;
    let path_map: HashMap<i64, String> = all_files.iter().map(|f| (f.id, f.path.clone())).collect();

    let dd_violations = if !all_constraints.is_empty() {
        compute_violations(
            dd_engine,
            db,
            &all_constraints,
            &path_map,
            &file_to_component,
            &comp_name_to_id,
        )
    } else {
        None
    };

    let mut orientation_sections = Vec::new();

    for comp in &components {
        let is_sketch = comp.lifecycle_state == "sketch";

        let in_scope: Vec<_> = all_conventions
            .iter()
            .filter(|c| {
                c.component_id.as_deref() == Some(&comp.id) || c.component_id.is_none()
            })
            .collect();

        let conv_ids: Vec<&str> = in_scope.iter().map(|c| c.id.as_str()).collect();
        let templates = db.templates_for_conventions(&conv_ids).unwrap_or_default();
        let template_map: HashMap<&str, &str> = templates
            .iter()
            .map(|t| (t.convention_id.as_str(), t.template_text.as_str()))
            .collect();
        let in_scope_waivers: Vec<_> = all_waivers
            .iter()
            .filter(|w| {
                conv_ids.contains(&w.convention_id.as_str())
                    && (w.component_id == comp.id || w.component_id.is_empty())
            })
            .collect();

        let in_scope_proposals: Vec<_> = all_proposals
            .iter()
            .filter(|p| conv_ids.contains(&p.convention_id.as_str()))
            .collect();

        let mut preferred = Vec::new();
        let mut deprecated = Vec::new();
        let mut forbidden = Vec::new();
        let mut descriptive = Vec::new();

        for c in &in_scope {
            let state = if is_sketch {
                "descriptive"
            } else {
                c.lifecycle_state.as_deref().unwrap_or("descriptive")
            };
            let mut entry = json!({
                "convention_id": c.id,
                "antecedent": c.antecedent.split(", ").collect::<Vec<_>>(),
                "consequent": c.consequent.split(", ").collect::<Vec<_>>(),
                "support": c.support,
                "confidence": c.confidence,
                "scope": if c.component_id.is_some() { "component" } else { "global" },
            });
            if let Some(tmpl) = template_map.get(c.id.as_str()) {
                entry["template"] = json!(tmpl);
            }
            match state {
                "preferred" => preferred.push(entry),
                "deprecated" => deprecated.push(entry),
                "forbidden" => forbidden.push(entry),
                _ => descriptive.push(entry),
            }
        }

        let drift = if is_sketch {
            None
        } else {
            check_drift_from_snapshots(db, &comp.id, &comp.name)
        };

        let mut section = json!({
            "component_id": comp.id,
            "component_name": comp.name,
            "lifecycle_state": comp.lifecycle_state,
        });

        if is_sketch {
            section["sketch_mode_note"] =
                json!("Component is in sketch mode — all conventions are informational only");
        }

        if !preferred.is_empty() {
            section["preferred"] = json!({
                "guidance": "Follow these conventions. Templates show the expected signature shape.",
                "conventions": preferred,
            });
        }
        if !deprecated.is_empty() {
            section["warnings"] = json!({
                "guidance": "Avoid these patterns — they are being phased out.",
                "conventions": deprecated,
            });
        }
        if !forbidden.is_empty() {
            section["anti_patterns"] = json!({
                "guidance": "Do not use these patterns.",
                "conventions": forbidden,
            });
        }
        if !descriptive.is_empty() {
            section["observed_patterns"] = json!({
                "guidance": "Observed patterns in this scope (informational only).",
                "conventions": descriptive,
            });
        }
        if let Some(drift_alert) = drift {
            section["drift_alert"] = drift_alert;
        }
        if !in_scope_waivers.is_empty() {
            section["active_waivers"] = json!(in_scope_waivers
                .iter()
                .map(|w| {
                    json!({
                        "waiver_id": w.id,
                        "convention_id": w.convention_id,
                        "symbol": w.symbol_qualified_name,
                        "rationale": w.rationale,
                        "waived_by": w.waived_by,
                        "waived_at": w.waived_at,
                    })
                })
                .collect::<Vec<_>>());
        }
        if !in_scope_proposals.is_empty() {
            section["pending_proposals"] = json!(in_scope_proposals
                .iter()
                .map(|p| {
                    json!({
                        "proposal_id": p.id,
                        "convention_id": p.convention_id,
                        "proposed_transition": p.proposed_transition,
                        "signal_rationale": p.signal_rationale,
                        "created_at": p.created_at,
                    })
                })
                .collect::<Vec<_>>());
        }

        // Constraint section
        let in_scope_constraints = constraints_for_component(
            &all_constraints,
            &comp.files,
            &comp.id,
            &comp_name_to_id,
        );

        if !in_scope_constraints.is_empty() {
            let constraint_ids: HashSet<&str> =
                in_scope_constraints.iter().map(|c| c.id.as_str()).collect();
            let file_set: HashSet<&str> = comp.files.iter().map(|f| f.as_str()).collect();

            let active: Vec<_> = in_scope_constraints
                .iter()
                .map(|c| {
                    let mut entry = json!({
                        "constraint_id": c.id,
                        "kind": c.kind.kind_tag(),
                        "severity": format!("{:?}", c.severity).to_lowercase(),
                        "detail": constraint_detail(c),
                    });
                    if let Some(name) = &c.name {
                        entry["name"] = json!(name);
                    }
                    if let Some(prov) = &c.provenance {
                        entry["provenance"] = json!(prov);
                    }
                    if let Some(s) = &c.scope {
                        entry["scope"] = json!(s);
                    }
                    entry
                })
                .collect();

            let mut constraints_section = json!({ "active": active });

            if let Some(violations) = &dd_violations {
                let in_scope_violations: Vec<_> = violations
                    .iter()
                    .filter(|v| {
                        constraint_ids.contains(v.constraint_id.as_str())
                            && (file_set.contains(v.from_path.as_str())
                                || file_set.contains(v.to_path.as_str()))
                    })
                    .map(|v| {
                        json!({
                            "constraint_id": v.constraint_id,
                            "constraint_name": v.constraint_name,
                            "from_path": v.from_path,
                            "to_path": v.to_path,
                            "severity": v.severity,
                            "detail": v.detail,
                        })
                    })
                    .collect();
                if !in_scope_violations.is_empty() {
                    constraints_section["violations"] = json!(in_scope_violations);
                }
            }

            let in_scope_constraint_waivers: Vec<_> = all_constraint_waivers
                .iter()
                .filter(|w| {
                    constraint_ids.contains(w.constraint_id.as_str())
                        && file_set.contains(w.file_path.as_str())
                })
                .map(|w| {
                    json!({
                        "waiver_id": w.id,
                        "constraint_id": w.constraint_id,
                        "constraint_name": w.constraint_name,
                        "file_path": w.file_path,
                        "rationale": w.rationale,
                        "waived_by": w.waived_by,
                    })
                })
                .collect();
            if !in_scope_constraint_waivers.is_empty() {
                constraints_section["waivers"] = json!(in_scope_constraint_waivers);
            }

            section["constraints"] = constraints_section;
        }

        orientation_sections.push(section);
    }

    Ok(json!({
        "scope": scope,
        "orientation": orientation_sections,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn setup_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_unchecked("orient_test", dir.path()).unwrap();
        (db, dir)
    }

    fn insert_component(db: &Db, id: &str, name: &str, files: &[&str]) {
        let paths_json = serde_json::to_string(&files).unwrap();
        let components: Vec<(String, String, String)> =
            vec![(id.into(), name.into(), paths_json)];
        db.batch_create_components(&components, &[]).unwrap();
    }

    fn insert_convention(db: &Db, id: &str, component_id: Option<&str>) {
        db.upsert_convention(id, "kind:function", "has_return_type", 5, 0.95, component_id)
            .unwrap();
    }

    #[test]
    fn resolve_scope_by_name() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "conventions", &["src/conventions/engine.rs"]);

        let results = resolve_scope(&db, "conventions").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "comp-1");
        assert_eq!(results[0].name, "conventions");
    }

    #[test]
    fn resolve_scope_by_name_case_insensitive() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "Conventions", &["src/conventions/engine.rs"]);

        let results = resolve_scope(&db, "conventions").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "comp-1");
    }

    #[test]
    fn resolve_scope_by_file() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "conventions", &["src/conventions/engine.rs"]);

        let results = resolve_scope(&db, "src/conventions/engine.rs").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "comp-1");
    }

    #[test]
    fn resolve_scope_not_found() {
        let (db, dir) = setup_db();
        let results = resolve_scope(&db, "nonexistent").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn handle_no_component_returns_error() {
        let (db, dir) = setup_db();
        let result = handle(&db, "nonexistent", dir.path(), None).unwrap();
        assert!(result["error"].as_str().unwrap().contains("no component found"));
    }

    #[test]
    fn handle_empty_conventions() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        assert_eq!(orientation["component_name"], "mycomp");
        assert!(orientation.get("preferred").is_none());
        assert!(orientation.get("warnings").is_none());
        assert!(orientation.get("observed_patterns").is_none());
    }

    #[test]
    fn handle_preferred_with_template() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));
        db.set_convention_lifecycle("conv-1", "preferred", None).unwrap();
        db.upsert_convention_template("conv-1", "pub fn $NAME(&self) -> Result<$T>", &[]).unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        let preferred = &orientation["preferred"]["conventions"];
        assert_eq!(preferred.as_array().unwrap().len(), 1);
        assert_eq!(preferred[0]["template"], "pub fn $NAME(&self) -> Result<$T>");
        assert_eq!(preferred[0]["scope"], "component");
    }

    #[test]
    fn handle_global_convention_included() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "global-1", None);
        db.set_convention_lifecycle("global-1", "preferred", None).unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        let preferred = &orientation["preferred"]["conventions"];
        assert_eq!(preferred[0]["scope"], "global");
    }

    #[test]
    fn handle_deprecated_warning() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));
        db.set_convention_lifecycle("conv-1", "deprecated", None).unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        assert!(orientation["warnings"]["conventions"].as_array().unwrap().len() == 1);
        assert!(orientation.get("preferred").is_none());
    }

    #[test]
    fn handle_forbidden_anti_pattern() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));
        db.set_convention_lifecycle("conv-1", "forbidden", None).unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        assert!(orientation["anti_patterns"]["conventions"].as_array().unwrap().len() == 1);
    }

    #[test]
    fn handle_descriptive_informational_only() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        let patterns = &orientation["observed_patterns"];
        assert!(patterns["guidance"].as_str().unwrap().contains("informational only"));
        assert_eq!(patterns["conventions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn handle_sketch_mode_flattens() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));
        db.set_convention_lifecycle("conv-1", "preferred", None).unwrap();
        db.set_component_lifecycle("comp-1", "sketch").unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        assert!(orientation.get("preferred").is_none());
        assert!(orientation["observed_patterns"]["conventions"].as_array().unwrap().len() == 1);
        assert!(orientation["sketch_mode_note"].as_str().is_some());
    }

    #[test]
    fn drift_from_snapshots_triggers_alert() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let dist_low = r#"{"kind:function": 0.8, "has_doc": 0.3}"#;
        let dist_mid = r#"{"kind:function": 0.7, "has_doc": 0.4}"#;
        let dist_high = r#"{"kind:function": 0.6, "has_doc": 0.5}"#;

        db.insert_convention_snapshot("comp-1", 1.0, 10, dist_low, "h1").unwrap();
        db.insert_convention_snapshot("comp-1", 1.1, 10, dist_mid, "h2").unwrap();
        db.insert_convention_snapshot("comp-1", 1.2, 10, dist_high, "h3").unwrap();

        let alert = check_drift_from_snapshots(&db, "comp-1", "mycomp");
        assert!(alert.is_some());
        let alert = alert.unwrap();
        assert_eq!(alert["component_id"], "comp-1");
        assert!(alert["delta"].as_f64().unwrap() > 0.15);
    }

    #[test]
    fn drift_no_alert_when_stable() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let dist = r#"{"kind:function": 0.8}"#;
        db.insert_convention_snapshot("comp-1", 1.0, 10, dist, "h1").unwrap();
        db.insert_convention_snapshot("comp-1", 1.0, 10, dist, "h2").unwrap();
        db.insert_convention_snapshot("comp-1", 1.0, 10, dist, "h3").unwrap();

        let alert = check_drift_from_snapshots(&db, "comp-1", "mycomp");
        assert!(alert.is_none());
    }

    #[test]
    fn waivers_in_scope() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));
        db.set_convention_lifecycle("conv-1", "preferred", None).unwrap();
        db.create_waiver("conv-1", "my_func", "comp-1", "intentional deviation", "josh")
            .unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        let waivers = orientation["active_waivers"].as_array().unwrap();
        assert_eq!(waivers.len(), 1);
        assert_eq!(waivers[0]["convention_id"], "conv-1");
        assert_eq!(waivers[0]["rationale"], "intentional deviation");
    }

    #[test]
    fn pending_proposals_surfaced() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));
        db.create_proposal("conv-1", "descriptive -> preferred", "stable high support", "promote")
            .unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        let proposals = orientation["pending_proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0]["convention_id"], "conv-1");
    }
}
