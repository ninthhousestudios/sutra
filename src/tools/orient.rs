use std::collections::{HashMap, HashSet};
use std::path::Path;

use glob::{MatchOptions, Pattern};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::components;
use crate::constraints::DdEngine;
use crate::constraints::check::{EvalScope, FactsSource};
use crate::conventions;
use crate::db::{Db, HealthFindingRow};
use crate::error::Result;
use crate::health::{findings::BiomarkerKind, scoring};
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
            let lifecycle = db
                .component_lifecycle_state(comp_id)
                .unwrap_or_else(|_| "stable".into());
            results.push(ResolvedComponent {
                id: comp_id.clone(),
                name: comp_name.clone(),
                lifecycle_state: lifecycle,
                files: paths.clone(),
            });
            return Ok(results);
        }
    }

    if let Ok(Some(alias)) = db.find_alias(scope)
        && alias.target_kind == "component"
    {
        let lifecycle = db
            .component_lifecycle_state(&alias.target_ref)
            .unwrap_or_else(|_| "stable".into());
        let (canon_name, files) = components
            .iter()
            .find(|(id, _, _)| id == &alias.target_ref)
            .map(|(_, name, p)| (name.clone(), p.clone()))
            .unwrap_or_else(|| (alias.term.clone(), vec![]));
        results.push(ResolvedComponent {
            id: alias.target_ref.clone(),
            name: canon_name,
            lifecycle_state: lifecycle,
            files,
        });
        return Ok(results);
    }

    for (comp_id, comp_name, paths) in &components {
        if paths.iter().any(|p| p == scope) {
            let lifecycle = db
                .component_lifecycle_state(comp_id)
                .unwrap_or_else(|_| "stable".into());
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
            let lifecycle = db
                .component_lifecycle_state(comp_id)
                .unwrap_or_else(|_| "stable".into());
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
    let snapshots = db
        .recent_convention_snapshots(component_id, conventions::DRIFT_WINDOW)
        .ok()?;
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
                        from_pat.as_ref().is_some_and(|p| p.matches_with(f, opts))
                            || to_pat.as_ref().is_some_and(|p| p.matches_with(f, opts))
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
                            .is_some_and(|id| id == component_id)
                        || comp_name_to_id
                            .get(to_component.as_str())
                            .is_some_and(|id| id == component_id)
                }
                ConstraintKind::MaxFanIn { target, .. } => {
                    component_files.iter().any(|f| f == target)
                }
                ConstraintKind::NoCycles => true,
                ConstraintKind::ForbiddenExternal { from, .. } => {
                    let from_pat = Pattern::new(from).ok();
                    component_files
                        .iter()
                        .any(|f| from_pat.as_ref().is_some_and(|p| p.matches_with(f, opts)))
                }
                // confinement applies everywhere outside allowed_in — always relevant
                ConstraintKind::ConfinedExternal { .. } => true,
            }
        })
        .collect()
}

fn hidden_coupling_for_component(
    db: &Db,
    component_id: &str,
    threshold: f64,
    path_map: &HashMap<i64, String>,
) -> Vec<serde_json::Value> {
    let file_ids: HashSet<i64> = match db.component_file_ids(component_id) {
        Ok(ids) => ids.into_iter().collect(),
        Err(_) => return Vec::new(),
    };
    if file_ids.len() < 2 {
        return Vec::new();
    }

    let cochange_pairs = match db.cochange_pairs_above_threshold(threshold) {
        Ok(pairs) => pairs,
        Err(_) => return Vec::new(),
    };

    let static_edges: HashSet<(i64, i64)> = db
        .static_file_edges()
        .unwrap_or_default()
        .into_iter()
        .filter(|(a, b)| file_ids.contains(a) && file_ids.contains(b))
        .collect();

    let mut entries: Vec<(f64, serde_json::Value)> = cochange_pairs
        .into_iter()
        .filter(|(fa, fb, _, _)| file_ids.contains(fa) && file_ids.contains(fb))
        .filter(|(fa, fb, _, _)| !static_edges.contains(&((*fa).min(*fb), (*fa).max(*fb))))
        .filter(|(fa, fb, _, _)| {
            let pa = path_map.get(fa).map(|s| s.as_str()).unwrap_or("");
            let pb = path_map.get(fb).map(|s| s.as_str()).unwrap_or("");
            components::is_test_file(pa) == components::is_test_file(pb)
        })
        .map(|(fa, fb, jaccard, shared)| {
            let file_a = path_map.get(&fa).cloned().unwrap_or_default();
            let file_b = path_map.get(&fb).cloned().unwrap_or_default();
            (
                jaccard,
                json!({
                    "file_a": file_a,
                    "file_b": file_b,
                    "jaccard": jaccard,
                    "shared_commits": shared,
                }),
            )
        })
        .collect();

    entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    entries.into_iter().map(|(_, v)| v).collect()
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
        ConstraintKind::ForbiddenExternal { from, crates, .. } => {
            format!("{from} must not depend on external [{}]", crates.join(", "))
        }
        ConstraintKind::ConfinedExternal {
            crates, allowed_in, ..
        } => {
            format!(
                "external [{}] allowed only in [{}]",
                crates.join(", "),
                allowed_in.join(", ")
            )
        }
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

    let components_config = components::load_config(workspace_root)?;
    let cochange_threshold = components_config.cochange_threshold.unwrap_or(0.5);

    let all_conventions = db.all_conventions_merged()?;
    let all_proposals = db.pending_proposals()?;
    let all_waivers = db.list_waivers(None).unwrap_or_default();

    // Constraint system
    let loaded_rules = rules::load_rules(workspace_root)?;
    let (all_constraints, constraint_parse_errors) = loaded_rules.all_constraints();
    let all_constraint_waivers = db.get_constraint_waivers(None).unwrap_or_default();

    let comp_with_paths = db.active_components_with_paths()?;
    let mut comp_name_to_id: HashMap<String, String> = HashMap::new();
    for (comp_id, name, _) in &comp_with_paths {
        comp_name_to_id.insert(name.clone(), comp_id.clone());
    }

    let all_files = db.all_files()?;
    let path_map: HashMap<i64, String> = all_files.iter().map(|f| (f.id, f.path.clone())).collect();

    let check_outcome = crate::constraints::check::evaluate(
        &FactsSource::DdBacked { db, dd_engine },
        workspace_root,
        EvalScope::Workspace,
    )
    .ok();

    // Health findings (load once, filter waived)
    let mut findings_by_file: HashMap<i64, Vec<HealthFindingRow>> = HashMap::new();
    if let Ok(all_findings_with_waivers) = db.get_health_findings_with_waiver_status() {
        for (finding, waived) in all_findings_with_waivers {
            if !waived {
                findings_by_file
                    .entry(finding.file_id)
                    .or_default()
                    .push(finding);
            }
        }
    }

    let workspace_health = scoring::score_workspace(db, true).ok();
    let comp_health_map: HashMap<&str, &scoring::ScoredComponent> = workspace_health
        .as_ref()
        .map(|wh| {
            wh.component_scores
                .iter()
                .map(|cs| (cs.component_id.as_str(), cs))
                .collect()
        })
        .unwrap_or_default();

    let mut orientation_sections = Vec::new();

    for comp in &components {
        let is_sketch = comp.lifecycle_state == "sketch";

        let in_scope: Vec<_> = all_conventions
            .iter()
            .filter(|c| c.component_id.as_deref() == Some(&comp.id) || c.component_id.is_none())
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
            section["active_waivers"] = json!(
                in_scope_waivers
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
                    .collect::<Vec<_>>()
            );
        }
        if !in_scope_proposals.is_empty() {
            section["pending_proposals"] = json!(
                in_scope_proposals
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
                    .collect::<Vec<_>>()
            );
        }

        // Constraint section
        let in_scope_constraints =
            constraints_for_component(&all_constraints, &comp.files, &comp.id, &comp_name_to_id);

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
                        "severity": c.severity.as_str(),
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

            if let Some(outcome) = &check_outcome {
                let all_findings = outcome
                    .active
                    .iter()
                    .chain(outcome.waived.iter().map(|w| &w.finding));
                let in_scope_violations: Vec<_> = all_findings
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
                            "severity": v.severity.as_str(),
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

        let hidden = hidden_coupling_for_component(db, &comp.id, cochange_threshold, &path_map);
        if !hidden.is_empty() {
            section["hidden_coupling"] = json!(hidden);
        }

        // Health section
        let mut health_section = json!({});
        let mut has_health = false;

        if let Some(cs) = comp_health_map.get(comp.id.as_str()) {
            health_section["health_score"] = json!(scoring::round2(cs.score));
            has_health = true;

            if let Some(inst) = &cs.instability {
                health_section["component_instability"] = json!({
                    "ce": inst.ce,
                    "ca": inst.ca,
                    "instability": scoring::round2(inst.instability),
                });
            }
        }

        // Top findings rendering (from raw findings, not scored rollup)
        let mut comp_findings: Vec<&HealthFindingRow> = workspace_health
            .as_ref()
            .and_then(|wh| wh.comp_file_ids.get(&comp.id))
            .into_iter()
            .flatten()
            .filter_map(|fid| findings_by_file.get(fid))
            .flatten()
            .collect();

        if !comp_findings.is_empty() {
            comp_findings.sort_by(|a, b| {
                a.severity.cmp(&b.severity).then_with(|| {
                    let wa = BiomarkerKind::parse(&a.biomarker_kind)
                        .map(|k| k.default_weight())
                        .unwrap_or(0.0);
                    let wb = BiomarkerKind::parse(&b.biomarker_kind)
                        .map(|k| k.default_weight())
                        .unwrap_or(0.0);
                    wb.partial_cmp(&wa).unwrap_or(std::cmp::Ordering::Equal)
                })
            });
            let top: Vec<_> = comp_findings
                .iter()
                .take(5)
                .map(|f| {
                    let mut entry = json!({
                        "biomarker": f.biomarker_kind,
                        "severity": f.severity,
                        "detail": f.detail,
                    });
                    if let Some(path) = path_map.get(&f.file_id) {
                        entry["file"] = json!(path);
                    }
                    entry
                })
                .collect();
            health_section["top_findings"] = json!(top);
            has_health = true;
        }

        if has_health {
            section["health"] = health_section;
        }

        orientation_sections.push(section);
    }

    let mut result = json!({
        "scope": scope,
        "orientation": orientation_sections,
    });

    if !constraint_parse_errors.is_empty() {
        result["constraint_parse_errors"] =
            json!(constraint_parse_errors
            .iter()
            .map(|e| {
                json!({
                    "severity": "blocking",
                    "index": e.index,
                    "name": e.name,
                    "error": e.error,
                    "detail": format!(
                        "malformed [[constraint]] at index {}{}: {}",
                        e.index,
                        e.name.as_deref().map(|n| format!(" (name: {n})")).unwrap_or_default(),
                        e.error,
                    ),
                })
            })
            .collect::<Vec<_>>());
    }

    Ok(result)
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
        let components: Vec<(String, String, String)> = vec![(id.into(), name.into(), paths_json)];
        db.batch_create_components(&components, &[]).unwrap();
    }

    fn insert_convention(db: &Db, id: &str, component_id: Option<&str>) {
        db.upsert_convention(
            id,
            "kind:function",
            "has_return_type",
            5,
            0.95,
            component_id,
        )
        .unwrap();
    }

    #[test]
    fn resolve_scope_by_name() {
        let (db, _dir) = setup_db();
        insert_component(&db, "comp-1", "conventions", &["src/conventions/engine.rs"]);

        let results = resolve_scope(&db, "conventions").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "comp-1");
        assert_eq!(results[0].name, "conventions");
    }

    #[test]
    fn resolve_scope_by_name_case_insensitive() {
        let (db, _dir) = setup_db();
        insert_component(&db, "comp-1", "Conventions", &["src/conventions/engine.rs"]);

        let results = resolve_scope(&db, "conventions").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "comp-1");
    }

    #[test]
    fn resolve_scope_by_file() {
        let (db, _dir) = setup_db();
        insert_component(&db, "comp-1", "conventions", &["src/conventions/engine.rs"]);

        let results = resolve_scope(&db, "src/conventions/engine.rs").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "comp-1");
    }

    #[test]
    fn resolve_scope_by_alias_uses_canonical_name() {
        let (db, _dir) = setup_db();
        insert_component(&db, "comp-1", "authentication", &["src/auth/mod.rs"]);
        db.replace_all_aliases(&[(
            "a1".into(),
            "auth".into(),
            "component".into(),
            "comp-1".into(),
        )])
        .unwrap();

        let results = resolve_scope(&db, "auth").unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "comp-1");
        assert_eq!(results[0].name, "authentication");
        assert_eq!(results[0].files, vec!["src/auth/mod.rs"]);
    }

    #[test]
    fn resolve_scope_not_found() {
        let (db, _dir) = setup_db();
        let results = resolve_scope(&db, "nonexistent").unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn handle_no_component_returns_error() {
        let (db, dir) = setup_db();
        let result = handle(&db, "nonexistent", dir.path(), None).unwrap();
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("no component found")
        );
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
        db.set_convention_lifecycle("conv-1", "preferred", None)
            .unwrap();
        db.upsert_convention_template("conv-1", "pub fn $NAME(&self) -> Result<$T>", &[])
            .unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        let preferred = &orientation["preferred"]["conventions"];
        assert_eq!(preferred.as_array().unwrap().len(), 1);
        assert_eq!(
            preferred[0]["template"],
            "pub fn $NAME(&self) -> Result<$T>"
        );
        assert_eq!(preferred[0]["scope"], "component");
    }

    #[test]
    fn handle_global_convention_included() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "global-1", None);
        db.set_convention_lifecycle("global-1", "preferred", None)
            .unwrap();

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
        db.set_convention_lifecycle("conv-1", "deprecated", None)
            .unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        assert!(
            orientation["warnings"]["conventions"]
                .as_array()
                .unwrap()
                .len()
                == 1
        );
        assert!(orientation.get("preferred").is_none());
    }

    #[test]
    fn handle_forbidden_anti_pattern() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));
        db.set_convention_lifecycle("conv-1", "forbidden", None)
            .unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        assert!(
            orientation["anti_patterns"]["conventions"]
                .as_array()
                .unwrap()
                .len()
                == 1
        );
    }

    #[test]
    fn handle_descriptive_informational_only() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        let patterns = &orientation["observed_patterns"];
        assert!(
            patterns["guidance"]
                .as_str()
                .unwrap()
                .contains("informational only")
        );
        assert_eq!(patterns["conventions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn handle_sketch_mode_flattens() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));
        db.set_convention_lifecycle("conv-1", "preferred", None)
            .unwrap();
        db.set_component_lifecycle("comp-1", "sketch").unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        assert!(orientation.get("preferred").is_none());
        assert!(
            orientation["observed_patterns"]["conventions"]
                .as_array()
                .unwrap()
                .len()
                == 1
        );
        assert!(orientation["sketch_mode_note"].as_str().is_some());
    }

    #[test]
    fn drift_from_snapshots_triggers_alert() {
        let (db, _dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let dist_low = r#"{"kind:function": 0.8, "has_doc": 0.3}"#;
        let dist_mid = r#"{"kind:function": 0.7, "has_doc": 0.4}"#;
        let dist_high = r#"{"kind:function": 0.6, "has_doc": 0.5}"#;

        db.insert_convention_snapshot("comp-1", 0.8, 10, dist_low, "h1", None, None)
            .unwrap();
        db.insert_convention_snapshot("comp-1", 1.0, 10, dist_mid, "h2", None, None)
            .unwrap();
        db.insert_convention_snapshot("comp-1", 1.2, 10, dist_high, "h3", None, None)
            .unwrap();

        let alert = check_drift_from_snapshots(&db, "comp-1", "mycomp");
        assert!(alert.is_some());
        let alert = alert.unwrap();
        assert_eq!(alert["component_id"], "comp-1");
        assert!(alert["delta"].as_f64().unwrap() > 0.3);
    }

    #[test]
    fn drift_no_alert_when_stable() {
        let (db, _dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let dist = r#"{"kind:function": 0.8}"#;
        db.insert_convention_snapshot("comp-1", 1.0, 10, dist, "h1", None, None)
            .unwrap();
        db.insert_convention_snapshot("comp-1", 1.0, 10, dist, "h2", None, None)
            .unwrap();
        db.insert_convention_snapshot("comp-1", 1.0, 10, dist, "h3", None, None)
            .unwrap();

        let alert = check_drift_from_snapshots(&db, "comp-1", "mycomp");
        assert!(alert.is_none());
    }

    #[test]
    fn waivers_in_scope() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        insert_convention(&db, "conv-1", Some("comp-1"));
        db.set_convention_lifecycle("conv-1", "preferred", None)
            .unwrap();
        db.create_waiver(
            "conv-1",
            "my_func",
            "comp-1",
            "intentional deviation",
            "josh",
        )
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
        db.create_proposal(
            "conv-1",
            "descriptive -> preferred",
            "stable high support",
            "promote",
        )
        .unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let orientation = &result["orientation"][0];
        let proposals = orientation["pending_proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0]["convention_id"], "conv-1");
    }

    fn write_rules(dir: &tempfile::TempDir, content: &str) {
        let sutra_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&sutra_dir).unwrap();
        std::fs::write(sutra_dir.join("rules.toml"), content).unwrap();
    }

    #[test]
    fn constraints_in_scope_by_path_prefix() {
        let (db, dir) = setup_db();
        insert_component(
            &db,
            "comp-1",
            "tools",
            &["src/tools/review.rs", "src/tools/orient.rs"],
        );
        write_rules(
            &dir,
            r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/tools/*"
to = "src/daemon.rs"
scope = "src/tools/"
name = "no-tool-daemon"
provenance = "docs/adr-001"
"#,
        );

        let result = handle(&db, "tools", dir.path(), None).unwrap();
        let section = &result["orientation"][0]["constraints"];
        assert!(!section.is_null());
        let active = section["active"].as_array().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0]["kind"], "forbidden_dep");
        assert_eq!(active[0]["severity"], "blocking");
        assert_eq!(active[0]["name"], "no-tool-daemon");
        assert_eq!(active[0]["provenance"], "docs/adr-001");
        assert_eq!(active[0]["scope"], "src/tools/");
        assert_eq!(active[0]["detail"], "src/tools/* -> src/daemon.rs");
    }

    #[test]
    fn constraints_in_scope_by_boundary() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-db", "db", &["src/db/mod.rs"]);
        insert_component(&db, "comp-http", "http", &["src/http/mod.rs"]);
        write_rules(
            &dir,
            r#"
[[constraint]]
kind = "boundary"
from_component = "db"
to_component = "http"
"#,
        );

        let result = handle(&db, "db", dir.path(), None).unwrap();
        let section = &result["orientation"][0]["constraints"];
        let active = section["active"].as_array().unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0]["kind"], "boundary");
        assert_eq!(active[0]["detail"], "db -> http");
    }

    #[test]
    fn constraints_in_scope_by_glob() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "tools", &["src/tools/review.rs"]);
        write_rules(
            &dir,
            r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/tools/*"
to = "src/config.rs"
"#,
        );

        let result = handle(&db, "tools", dir.path(), None).unwrap();
        let active = result["orientation"][0]["constraints"]["active"]
            .as_array()
            .unwrap();
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn constraint_out_of_scope_excluded() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "tools", &["src/tools/review.rs"]);
        write_rules(
            &dir,
            r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/db/*"
to = "src/http/*"
scope = "src/db/"
"#,
        );

        let result = handle(&db, "tools", dir.path(), None).unwrap();
        assert!(result["orientation"][0]["constraints"].is_null());
    }

    #[test]
    fn constraint_waivers_shown() {
        let (db, dir) = setup_db();
        insert_component(
            &db,
            "comp-1",
            "tools",
            &["src/tools/review.rs", "src/tools/orient.rs"],
        );
        write_rules(
            &dir,
            r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/tools/*"
to = "src/daemon.rs"
name = "no-tool-daemon"
"#,
        );

        let (constraints, _) = rules::load_rules(dir.path()).unwrap().all_constraints();
        let constraint_id = &constraints[0].id;

        db.create_constraint_waiver(
            constraint_id,
            Some("no-tool-daemon"),
            "src/tools/review.rs",
            None,
            "temporary during migration",
            "josh",
        )
        .unwrap();

        let result = handle(&db, "tools", dir.path(), None).unwrap();
        let section = &result["orientation"][0]["constraints"];
        let waivers = section["waivers"].as_array().unwrap();
        assert_eq!(waivers.len(), 1);
        assert_eq!(waivers[0]["constraint_id"], constraint_id.as_str());
        assert_eq!(waivers[0]["rationale"], "temporary during migration");
        assert_eq!(waivers[0]["waived_by"], "josh");
    }

    #[test]
    fn no_constraints_section_absent() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        assert!(result["orientation"][0]["constraints"].is_null());
    }

    #[test]
    fn constraints_with_violations() {
        let (db, dir) = setup_db();
        insert_component(
            &db,
            "comp-1",
            "tools",
            &["src/tools/review.rs", "src/tools/orient.rs"],
        );

        let review_id = db
            .upsert_file("src/tools/review.rs", "rs", "abc123", 100, true)
            .unwrap();
        db.upsert_file("src/tools/orient.rs", "rs", "def456", 80, true)
            .unwrap();
        let daemon_id = db
            .upsert_file("src/daemon.rs", "rs", "ghi789", 50, true)
            .unwrap();

        db.insert_import(review_id, "src/daemon.rs", Some(daemon_id), 1)
            .unwrap();

        write_rules(
            &dir,
            r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/tools/*"
to = "src/daemon.rs"
name = "no-tool-daemon"
"#,
        );

        let engine = DdEngine::new(std::time::Duration::from_secs(60));
        let result = handle(&db, "tools", dir.path(), Some(&engine)).unwrap();
        let section = &result["orientation"][0]["constraints"];
        let violations = section["violations"].as_array().unwrap();
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0]["from_path"], "src/tools/review.rs");
        assert_eq!(violations[0]["to_path"], "src/daemon.rs");
        assert!(
            violations[0]["detail"]
                .as_str()
                .unwrap()
                .contains("forbidden")
        );
    }

    #[test]
    fn sketch_mode_constraints_still_shown() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        db.set_component_lifecycle("comp-1", "sketch").unwrap();
        write_rules(
            &dir,
            r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/*"
to = "src/banned.rs"
"#,
        );

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let section = &result["orientation"][0];
        assert!(section["sketch_mode_note"].as_str().is_some());
        let active = section["constraints"]["active"].as_array().unwrap();
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn hidden_coupling_surfaces_cochange_without_import() {
        let (db, dir) = setup_db();

        let id_a = db.upsert_file("src/a.rs", "rs", "aaa", 50, true).unwrap();
        let id_b = db.upsert_file("src/b.rs", "rs", "bbb", 50, true).unwrap();
        let id_c = db.upsert_file("src/c.rs", "rs", "ccc", 50, true).unwrap();

        let paths_json = serde_json::to_string(&["src/a.rs", "src/b.rs", "src/c.rs"]).unwrap();
        let components = vec![("comp-1".into(), "mycomp".into(), paths_json)];
        let membership = vec![
            ("comp-1".into(), id_a),
            ("comp-1".into(), id_b),
            ("comp-1".into(), id_c),
        ];
        db.batch_create_components(&components, &membership)
            .unwrap();

        // a and b co-change in all 3 commits, c only in 1
        let commits = vec![
            crate::db::CommitRow {
                hash: "h1".into(),
                committed_at: 1,
                author: "x".into(),
            },
            crate::db::CommitRow {
                hash: "h2".into(),
                committed_at: 2,
                author: "x".into(),
            },
            crate::db::CommitRow {
                hash: "h3".into(),
                committed_at: 3,
                author: "x".into(),
            },
        ];
        let pairs = vec![
            ("h1".into(), id_a),
            ("h1".into(), id_b),
            ("h1".into(), id_c),
            ("h2".into(), id_a),
            ("h2".into(), id_b),
            ("h3".into(), id_a),
            ("h3".into(), id_b),
        ];
        db.replace_commit_files(&commits, &pairs).unwrap();

        // a imports c — that pair should be excluded
        db.insert_import(id_a, "src/c.rs", Some(id_c), 1).unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let section = &result["orientation"][0];
        let coupling = section["hidden_coupling"].as_array().unwrap();

        // a-b: jaccard = 3/3 = 1.0, no import edge → included
        assert_eq!(coupling.len(), 1);
        assert_eq!(coupling[0]["file_a"], "src/a.rs");
        assert_eq!(coupling[0]["file_b"], "src/b.rs");
        assert_eq!(coupling[0]["jaccard"], 1.0);
        assert_eq!(coupling[0]["shared_commits"], 3);
    }

    #[test]
    fn hidden_coupling_excludes_test_production_pairs() {
        let (db, dir) = setup_db();

        let id_src = db.upsert_file("src/lib.rs", "rs", "aaa", 50, true).unwrap();
        let id_test = db
            .upsert_file("tests/lib_test.rs", "rs", "bbb", 50, true)
            .unwrap();

        let paths_json = serde_json::to_string(&["src/lib.rs", "tests/lib_test.rs"]).unwrap();
        let components = vec![("comp-1".into(), "mycomp".into(), paths_json)];
        let membership = vec![("comp-1".into(), id_src), ("comp-1".into(), id_test)];
        db.batch_create_components(&components, &membership)
            .unwrap();

        let commits = vec![
            crate::db::CommitRow {
                hash: "h1".into(),
                committed_at: 1,
                author: "x".into(),
            },
            crate::db::CommitRow {
                hash: "h2".into(),
                committed_at: 2,
                author: "x".into(),
            },
        ];
        let pairs = vec![
            ("h1".into(), id_src),
            ("h1".into(), id_test),
            ("h2".into(), id_src),
            ("h2".into(), id_test),
        ];
        db.replace_commit_files(&commits, &pairs).unwrap();

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        let section = &result["orientation"][0];
        assert!(section.get("hidden_coupling").is_none());
    }

    #[test]
    fn hidden_coupling_absent_when_no_cochange() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let result = handle(&db, "mycomp", dir.path(), None).unwrap();
        assert!(result["orientation"][0].get("hidden_coupling").is_none());
    }
}
