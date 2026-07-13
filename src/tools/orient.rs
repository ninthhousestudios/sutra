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
use crate::lessons::LessonsDb;
use crate::parser::adapter::LanguageRegistry;
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

fn constraints_for_component<'a>(
    all_constraints: &'a [Constraint],
    component_files: &[String],
    component_id: &str,
    comp_name_to_id: &HashMap<&str, &str>,
) -> Vec<&'a Constraint> {
    let opts = MatchOptions {
        require_literal_separator: true,
        ..MatchOptions::default()
    };
    all_constraints
        .iter()
        .filter(|c| {
            if let Some(scope) = &c.scope {
                return component_files
                    .iter()
                    .any(|f| crate::rules::scope_matches_path(scope, f));
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
                            .is_some_and(|id| *id == component_id)
                        || comp_name_to_id
                            .get(to_component.as_str())
                            .is_some_and(|id| *id == component_id)
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
                ConstraintKind::ForbiddenPattern { .. } => true,
            }
        })
        .collect()
}

fn hidden_coupling_for_component(
    db: &Db,
    component_id: &str,
    threshold: f64,
    path_map: &HashMap<i64, &str>,
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
            let pa = path_map.get(fa).copied().unwrap_or("");
            let pb = path_map.get(fb).copied().unwrap_or("");
            components::is_test_file(pa) == components::is_test_file(pb)
        })
        .map(|(fa, fb, jaccard, shared)| {
            let file_a = path_map.get(&fa).copied().unwrap_or("");
            let file_b = path_map.get(&fb).copied().unwrap_or("");
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
        ConstraintKind::ForbiddenPattern { language, .. } => {
            format!("forbidden pattern ({language})")
        }
    }
}

fn extract_component_sym_attrs(
    db: &Db,
    component_files: &[String],
    all_files: &[crate::db::FileRow],
    registry: &LanguageRegistry,
) -> Result<Vec<conventions::SymbolAttrs>> {
    let file_set: HashSet<&str> = component_files.iter().map(|s| s.as_str()).collect();
    let mut sym_attrs = Vec::new();

    for f in all_files {
        if !file_set.contains(&*f.path) {
            continue;
        }
        let syms = db.find_symbols_by_file(f.id)?;
        let refs = db.find_refs_in_file(f.id)?;

        let target_ids: Vec<i64> = refs
            .iter()
            .filter(|r| r.context_kind == "call")
            .filter_map(|r| r.target_symbol_id)
            .collect();
        let mut callee_cache: HashMap<i64, conventions::ResolvedCallee> = HashMap::new();
        for id in &target_ids {
            if !callee_cache.contains_key(id)
                && let Some(sym) = db.symbol_by_id(*id)?
            {
                callee_cache.insert(
                    *id,
                    conventions::ResolvedCallee {
                        qualified_name: sym.qualified_name.to_string(),
                        signature: sym.signature,
                    },
                );
            }
        }

        let dart_import_packages = if f.language == "dart" {
            conventions::dart_effect_packages(&db.imports_for_file(f.id)?)
        } else {
            None
        };

        for s in &syms {
            if let Some(mut attrs) =
                conventions::extract_attrs_for_symbol(s, &f.path, &f.language, registry)
            {
                if let Some(adapter) = registry.adapter_for_language(&f.language)
                    && let Some(fca_source) = adapter.as_fca_source()
                {
                    conventions::enrich_all_effects(
                        &mut attrs,
                        s,
                        &refs,
                        &callee_cache,
                        fca_source,
                        dart_import_packages.as_ref(),
                    );
                }
                sym_attrs.push(attrs);
            }
        }
    }

    Ok(sym_attrs)
}

pub fn handle(
    db: &Db,
    scope: &str,
    workspace_root: &Path,
    dd_engine: Option<&DdEngine>,
    lessons_db: Option<&LessonsDb>,
    registry: &LanguageRegistry,
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

    let toolchain_pairs = super::review::collect_toolchain_pairs(registry);

    // Constraint system
    let mut loaded_rules = rules::load_rules(workspace_root)?;
    let (all_constraints, constraint_parse_errors) = loaded_rules.all_constraints();
    let all_constraint_waivers = db.get_constraint_waivers(None).unwrap_or_default();

    let comp_with_paths = db.active_components_with_paths()?;
    let mut comp_name_to_id: HashMap<&str, &str> = HashMap::new();
    for (comp_id, name, _) in &comp_with_paths {
        comp_name_to_id.insert(name, comp_id);
    }

    let all_files = db.all_files()?;
    let path_map: HashMap<i64, &str> = all_files.iter().map(|f| (f.id, &*f.path)).collect();

    let check_outcome = crate::constraints::check::evaluate(
        &FactsSource::DdBacked { db, dd_engine },
        workspace_root,
        EvalScope::Workspace,
        registry,
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

    let ws_langs = lessons_db
        .map(|_| db.distinct_languages().unwrap_or_default())
        .unwrap_or_default();

    let mut orientation_sections = Vec::new();

    for comp in &components {
        let is_sketch = comp.lifecycle_state == "sketch";

        let mut section = json!({
            "component_id": comp.id,
            "component_name": comp.name,
            "lifecycle_state": comp.lifecycle_state,
        });

        if is_sketch {
            section["sketch_mode_note"] =
                json!("Component is in sketch mode — all conventions are informational only");
        }

        let sym_attrs = extract_component_sym_attrs(db, &comp.files, &all_files, registry)?;
        let patterns = conventions::describe_patterns(&sym_attrs, Some(&comp.id), &toolchain_pairs);
        if !patterns.is_empty() {
            let pattern_json: Vec<_> = patterns
                .iter()
                .map(|p| {
                    json!({
                        "pattern": format!("{} → {}",
                            p.antecedent.join(", "),
                            p.consequent.join(", ")),
                        "evidence": format!("{}/{} conform", p.support, p.total_matching),
                        "support": p.support,
                        "confidence": p.confidence,
                        "exemplars": p.exemplars,
                    })
                })
                .collect();
            section["observed_patterns"] = json!(pattern_json);
        }

        // Constraint section
        let in_scope_constraints =
            constraints_for_component(&all_constraints, &comp.files, &comp.id, &comp_name_to_id);

        if !in_scope_constraints.is_empty() {
            let constraint_ids: HashSet<&str> =
                in_scope_constraints.iter().map(|c| &*c.id).collect();
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
                        v.constraint_kind == "ratchet_violation"
                            || (constraint_ids.contains(&*v.constraint_id)
                                && (file_set.contains(v.from_path.as_str())
                                    || file_set.contains(v.to_path.as_str())))
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
                    constraint_ids.contains(&*w.constraint_id)
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

        if let Some(ldb) = lessons_db {
            let project_slug = workspace_root.file_name().and_then(|n| n.to_str());
            let mut seen_ids = HashSet::new();
            let mut comp_lessons = Vec::new();
            for file_path in &comp.files {
                let ctx = crate::lessons::MatchContext {
                    symbol_name: "",
                    file_path: Some(file_path),
                    imports: &[],
                    project: project_slug,
                    workspace_languages: &ws_langs,
                };
                let cl = ldb.query_for_context(&ctx)?;
                for lesson in cl.lessons {
                    if seen_ids.insert(lesson.id.clone()) {
                        comp_lessons.push(lesson);
                    }
                }
            }
            if !comp_lessons.is_empty() {
                let resolver = super::remember::build_hash_resolver(db);
                let _ = ldb.apply_staleness(&mut comp_lessons, &resolver);
                section["lessons"] = serde_json::to_value(&comp_lessons).unwrap_or_default();
            }
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
    use crate::db::{Db, InsertSymbolParams};

    fn setup_db() -> (Db, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_unchecked("orient_test", dir.path()).unwrap();
        (db, dir)
    }

    fn registry() -> LanguageRegistry {
        crate::parser::adapter::default_registry()
    }

    fn insert_component(db: &Db, id: &str, name: &str, files: &[&str]) {
        let paths_json = serde_json::to_string(&files).unwrap();
        let components: Vec<(String, String, String)> = vec![(id.into(), name.into(), paths_json)];
        db.batch_create_components(&components, &[]).unwrap();
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
        let result = handle(&db, "nonexistent", dir.path(), None, None, &registry()).unwrap();
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

        let result = handle(&db, "mycomp", dir.path(), None, None, &registry()).unwrap();
        let orientation = &result["orientation"][0];
        assert_eq!(orientation["component_name"], "mycomp");
        assert!(orientation.get("preferred").is_none());
        assert!(orientation.get("warnings").is_none());
        assert!(orientation.get("observed_patterns").is_none());
    }

    fn insert_pub_function(db: &Db, file_id: i64, name: &str, has_doc: bool) -> i64 {
        db.insert_symbol(&InsertSymbolParams {
            file_id,
            qualified_name: name,
            short_name: name,
            kind: "function",
            signature: Some("fn() -> Result<()>"),
            signature_hash: None,
            structural_hash: None,
            visibility: Some("pub"),
            start_line: 1,
            start_col: 0,
            end_line: 10,
            end_col: 0,
            parent_symbol_id: None,
            docstring: if has_doc { Some("docs") } else { None },
            cyclomatic: Some(2),
            cognitive: Some(1),
            max_nesting: None,
            flags: 0,
            language_attrs: None,
        })
        .unwrap()
    }

    #[test]
    fn handle_observed_patterns_from_fca() {
        let (db, dir) = setup_db();
        let file_id = db
            .upsert_file("src/lib.rs", "rust", "h1", 100, true)
            .unwrap();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        for i in 0..5 {
            insert_pub_function(&db, file_id, &format!("func_{i}"), true);
        }

        let result = handle(&db, "mycomp", dir.path(), None, None, &registry()).unwrap();
        let orientation = &result["orientation"][0];
        let patterns = orientation["observed_patterns"].as_array().unwrap();
        assert!(!patterns.is_empty());
        let first = &patterns[0];
        assert!(first["pattern"].as_str().unwrap().contains('→'));
        assert!(first["evidence"].as_str().unwrap().contains("conform"));
        assert!(first["exemplars"].as_array().is_some());
        assert!(first["support"].as_u64().unwrap() >= 2);
    }

    #[test]
    fn handle_no_patterns_without_symbols() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let result = handle(&db, "mycomp", dir.path(), None, None, &registry()).unwrap();
        let orientation = &result["orientation"][0];
        assert!(orientation.get("observed_patterns").is_none());
    }

    #[test]
    fn handle_sketch_mode_note() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);
        db.set_component_lifecycle("comp-1", "sketch").unwrap();

        let result = handle(&db, "mycomp", dir.path(), None, None, &registry()).unwrap();
        let orientation = &result["orientation"][0];
        assert!(orientation["sketch_mode_note"].as_str().is_some());
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

        let result = handle(&db, "tools", dir.path(), None, None, &registry()).unwrap();
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

        let result = handle(&db, "db", dir.path(), None, None, &registry()).unwrap();
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

        let result = handle(&db, "tools", dir.path(), None, None, &registry()).unwrap();
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

        let result = handle(&db, "tools", dir.path(), None, None, &registry()).unwrap();
        assert!(result["orientation"][0]["constraints"].is_null());
    }

    #[test]
    fn no_cycles_glob_scope_in_scope_for_component() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "tools", &["src/tools/review.rs"]);
        write_rules(
            &dir,
            r#"
[[constraint]]
kind = "no_cycles"
scope = "src/**"
name = "wrapper-no-cycles"
"#,
        );

        let result = handle(&db, "tools", dir.path(), None, None, &registry()).unwrap();
        let active = result["orientation"][0]["constraints"]["active"]
            .as_array()
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0]["kind"], "no_cycles");
        assert_eq!(active[0]["scope"], "src/**");
    }

    #[test]
    fn no_cycles_glob_scope_excluded_when_component_outside() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "tests", &["tests/it.rs"]);
        write_rules(
            &dir,
            r#"
[[constraint]]
kind = "no_cycles"
scope = "src/**"
"#,
        );

        let result = handle(&db, "tests", dir.path(), None, None, &registry()).unwrap();
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

        let result = handle(&db, "tools", dir.path(), None, None, &registry()).unwrap();
        let section = &result["orientation"][0]["constraints"];
        let waivers = section["waivers"].as_array().unwrap();
        assert_eq!(waivers.len(), 1);
        assert_eq!(waivers[0]["constraint_id"], &**constraint_id);
        assert_eq!(waivers[0]["rationale"], "temporary during migration");
        assert_eq!(waivers[0]["waived_by"], "josh");
    }

    #[test]
    fn no_constraints_section_absent() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let result = handle(&db, "mycomp", dir.path(), None, None, &registry()).unwrap();
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

        db.insert_import(
            review_id,
            "src/daemon.rs",
            Some(daemon_id),
            1,
            "import",
            None,
        )
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
        let result = handle(&db, "tools", dir.path(), Some(&engine), None, &registry()).unwrap();
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

        let result = handle(&db, "mycomp", dir.path(), None, None, &registry()).unwrap();
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
        db.insert_import(id_a, "src/c.rs", Some(id_c), 1, "import", None)
            .unwrap();

        let result = handle(&db, "mycomp", dir.path(), None, None, &registry()).unwrap();
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

        let result = handle(&db, "mycomp", dir.path(), None, None, &registry()).unwrap();
        let section = &result["orientation"][0];
        assert!(section.get("hidden_coupling").is_none());
    }

    #[test]
    fn hidden_coupling_absent_when_no_cochange() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let result = handle(&db, "mycomp", dir.path(), None, None, &registry()).unwrap();
        assert!(result["orientation"][0].get("hidden_coupling").is_none());
    }

    #[test]
    fn orient_includes_lessons_per_component() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let ldb = crate::lessons::LessonsDb::open(dir.path()).unwrap();
        ldb.store(&crate::lessons::StoreLessonParams {
            text: "Watch out for re-exports in lib.rs",
            anchors: &[(crate::lessons::AnchorKind::File, "src/lib.rs")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

        let result = handle(&db, "mycomp", dir.path(), None, Some(&ldb), &registry()).unwrap();
        let section = &result["orientation"][0];
        let lessons = section["lessons"].as_array().unwrap();
        assert_eq!(lessons.len(), 1);
        assert!(lessons[0]["text"].as_str().unwrap().contains("re-exports"));
    }

    #[test]
    fn orient_no_lessons_key_when_none_match() {
        let (db, dir) = setup_db();
        insert_component(&db, "comp-1", "mycomp", &["src/lib.rs"]);

        let ldb = crate::lessons::LessonsDb::open(dir.path()).unwrap();
        ldb.store(&crate::lessons::StoreLessonParams {
            text: "Unrelated lesson",
            anchors: &[(crate::lessons::AnchorKind::File, "src/unrelated/*.rs")],
            categories: &[],
            source_task_ids: &[],
            project_origin: None,
        })
        .unwrap();

        let result = handle(&db, "mycomp", dir.path(), None, Some(&ldb), &registry()).unwrap();
        assert!(result["orientation"][0].get("lessons").is_none());
    }
}
