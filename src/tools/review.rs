use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::components;
use crate::db::Db;
use crate::constraints::{
    self, ConstraintResolver, DdDelta, DdEngine, DdFacts,
};
use crate::error::Result;
use crate::conventions::{self, FcaEngine};
use crate::parser::adapter::LanguageRegistry;
use crate::freshness::{self, FreshnessLevel};
use crate::git;
use crate::rules::{self, match_no_cycles_constraint};
use crate::tools::scoring::{self, ChurnMap, Signal};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReviewArgs {
    pub workspace: String,
    /// "branch" (default), "staged", "unstaged", or a commit spec (e.g. "HEAD~3..HEAD", "abc123")
    #[serde(default)]
    pub diff: Option<String>,
}

const MAX_AFFECTED: usize = 20;
const MAX_READS: usize = 10;

const W_BLAST: f64 = 0.30;
const W_COMPLEXITY: f64 = 0.20;
const W_HOTSPOT: f64 = 0.15;
const W_CHURN: f64 = 0.15;
const W_CONVENTIONS: f64 = 0.20;

#[derive(Clone)]
pub struct ConstraintViolation {
    pub constraint_id: String,
    pub constraint_name: Option<String>,
    pub constraint_kind: String,
    pub severity: String,
    pub provenance: Option<String>,
    pub from_path: String,
    pub to_path: String,
    pub component_context: Option<String>,
    pub detail: String,
}

#[derive(Clone)]
pub struct WaivedConstraintViolation {
    pub constraint_id: String,
    pub constraint_name: Option<String>,
    pub constraint_kind: String,
    pub severity: String,
    pub provenance: Option<String>,
    pub from_path: String,
    pub to_path: String,
    pub component_context: Option<String>,
    pub detail: String,
    pub rationale: String,
    pub waived_by: String,
}

#[derive(Clone)]
pub struct ConventionViolation {
    pub symbol: String,
    pub file: String,
    pub convention_id: String,
    pub antecedent: Vec<String>,
    pub consequent: Vec<String>,
    pub missing: Vec<String>,
    pub support: usize,
    pub confidence: f64,
}

#[derive(Clone)]
pub struct ConventionMatchFinding {
    pub symbol: String,
    pub file: String,
    pub convention_id: String,
    pub antecedent: Vec<String>,
    pub consequent: Vec<String>,
    pub lifecycle_state: String,
    pub support: usize,
    pub confidence: f64,
}

#[derive(Clone)]
pub struct WaivedViolation {
    pub symbol: String,
    pub file: String,
    pub convention_id: String,
    pub antecedent: Vec<String>,
    pub consequent: Vec<String>,
    pub missing: Vec<String>,
    pub support: usize,
    pub confidence: f64,
    pub rationale: String,
}

#[derive(Default)]
pub struct ReviewFindings {
    pub constraint_violations: Vec<ConstraintViolation>,
    pub resolved_constraint_violations: Vec<ConstraintViolation>,
    pub waived_constraint_violations: Vec<WaivedConstraintViolation>,
    pub constraint_violations_total: usize,
    pub convention_violations: Vec<ConventionViolation>,
    pub convention_matches: Vec<ConventionMatchFinding>,
    pub waived_violations: Vec<WaivedViolation>,
    pub drift_alerts: Vec<conventions::drift::DriftAlert>,
    pub convention_drift_findings: Vec<crate::health::HealthFinding>,
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    diff_mode: Option<&str>,
    dd_engine: Option<&DdEngine>,
) -> Result<serde_json::Value> {
    let mode = diff_mode.unwrap_or("branch");

    let (changed_paths, base_revision, head_revision) = match mode {
        "staged" => (git::git_diff_staged(workspace_root)?, "HEAD".to_string(), Some(String::new())),
        "unstaged" => (git::git_diff_unstaged(workspace_root)?, "HEAD".to_string(), None),
        "branch" => {
            let default_branch = git::detect_default_branch(workspace_root)?;
            let base = git::git_merge_base(workspace_root, &default_branch)?;
            let paths = git::git_diff_files(workspace_root, &base, "HEAD")?;
            (paths, base, Some("HEAD".to_string()))
        }
        spec => {
            let (base, head) = if let Some((a, b)) = spec.split_once("..") {
                (a.to_string(), b.to_string())
            } else {
                (format!("{spec}~1"), spec.to_string())
            };
            let paths = git::git_diff_files(workspace_root, &base, &head)?;
            (paths, base, Some(head))
        }
    };

    let churn = ChurnMap {
        counts: git::git_churn(workspace_root, scoring::CHURN_WINDOW_DAYS)?,
        window_days: scoring::CHURN_WINDOW_DAYS,
    };

    let registry = crate::parser::adapter::default_registry();
    let (findings, findings_error) =
        match build_findings(db, workspace_root, &changed_paths, dd_engine, &registry) {
            Ok(f) => (f, None),
            Err(e) => (ReviewFindings::default(), Some(e.to_string())),
        };

    let shape_changes = crate::similarity::diff::detect_shape_changes(
        db,
        workspace_root,
        &changed_paths,
        &base_revision,
        head_revision.as_deref(),
        &registry,
        &crate::similarity::diff::ShapeChangeConfig::default(),
    );

    let ondemand_findings =
        crate::health::ondemand::compute_ondemand_findings(db, workspace_root, &changed_paths);

    let health_delta =
        crate::health::ondemand::compute_health_delta(db, &changed_paths, &ondemand_findings).ok();

    let mut result = compute(db, workspace_root, &changed_paths, &churn, &findings)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("diff_mode".into(), json!(mode));
        obj.insert(
            "churn_window_days".into(),
            json!(scoring::CHURN_WINDOW_DAYS),
        );
        if let Some(err) = findings_error {
            obj.insert("findings_degraded".into(), json!(true));
            obj.insert("findings_error".into(), json!(err));
            obj.insert("risk_score".into(), json!(null));
        }

        if !ondemand_findings.is_empty() {
            let health_out: Vec<_> = ondemand_findings
                .iter()
                .map(|f| {
                    json!({
                        "biomarker": f.biomarker_kind.as_str(),
                        "severity": f.severity.as_str(),
                        "file_id": f.file_id,
                        "symbol_id": f.symbol_id,
                        "metric_value": scoring::round3(f.metric_value),
                        "threshold": scoring::round3(f.threshold),
                        "detail": f.detail,
                    })
                })
                .collect();
            obj.insert("health_findings".into(), json!(health_out));
        }

        let shape_out: Vec<_> = shape_changes
            .iter()
            .filter(|c| c.quadrant == crate::similarity::diff::DiffQuadrant::SubtleStructural)
            .map(|c| {
                json!({
                    "file": c.file_path,
                    "symbol": c.symbol_name,
                    "text_delta": scoring::round3(c.text_delta),
                    "hrr_delta": scoring::round3(c.hrr_delta),
                    "quadrant": c.quadrant.as_str(),
                    "detail": format!(
                        "{}: text changed {:.0}% but structural shape changed {:.0}%",
                        c.symbol_name, c.text_delta * 100.0, c.hrr_delta * 100.0,
                    ),
                })
            })
            .collect();
        if !shape_out.is_empty() {
            obj.insert("hrr_shape_changes".into(), json!(shape_out));
        }

        if let Some(delta) = health_delta {
            let degraded_out: Vec<_> = delta
                .degraded
                .iter()
                .map(|e| {
                    let drivers: Vec<_> = e
                        .driving_findings
                        .iter()
                        .map(|f| {
                            json!({
                                "biomarker": f.biomarker_kind,
                                "detail": f.detail,
                            })
                        })
                        .collect();
                    json!({
                        "path": e.path,
                        "from": scoring::round3(e.previous_score),
                        "to": scoring::round3(e.current_score),
                        "delta": scoring::round3(e.delta),
                        "driving_findings": drivers,
                    })
                })
                .collect();
            let improved_out: Vec<_> = delta
                .improved
                .iter()
                .map(|e| {
                    json!({
                        "path": e.path,
                        "from": scoring::round3(e.previous_score),
                        "to": scoring::round3(e.current_score),
                        "delta": scoring::round3(e.delta),
                    })
                })
                .collect();
            if !degraded_out.is_empty() || !improved_out.is_empty() {
                obj.insert(
                    "health_delta".into(),
                    json!({
                        "degraded": degraded_out,
                        "improved": improved_out,
                    }),
                );
            }
        }
    }
    Ok(result)
}

pub fn build_findings(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    shared_dd: Option<&DdEngine>,
    registry: &LanguageRegistry,
) -> Result<ReviewFindings> {
    let rules = rules::load_rules(workspace_root)?;
    let all_files = db.all_files()?;
    let path_map: HashMap<i64, String> = all_files.iter().map(|f| (f.id, f.path.clone())).collect();
    let id_map: HashMap<&str, i64> = all_files.iter().map(|f| (f.path.as_str(), f.id)).collect();

    // File-to-component mapping (used for both constraint and convention processing)
    let comp_with_paths = db.active_components_with_paths()?;
    let mut file_to_component: HashMap<String, String> = HashMap::new();
    let mut comp_name_to_id: HashMap<String, String> = HashMap::new();
    for (comp_id, name, paths) in &comp_with_paths {
        comp_name_to_id.insert(name.clone(), comp_id.clone());
        for path in paths {
            file_to_component.insert(path.clone(), comp_id.clone());
        }
    }

    // DD: constraint violations via maintained view + cycle detection
    let all_constraints = rules.all_constraints()?;
    let mut constraint_violations = Vec::new();
    let mut resolved_constraint_violations = Vec::new();
    let mut constraint_violations_total: usize = 0;

    let ephemeral;
    let edges = db.import_edges()?;
    let dd: Option<&DdEngine> = if !edges.is_empty() {
        if let Some(engine) = shared_dd {
            if engine.is_invalidated() {
                engine.reload(DdFacts {
                    import_edges: edges.clone(),
                });
            } else if !engine.is_loaded() {
                engine.ingest(DdFacts {
                    import_edges: edges.clone(),
                })?;
            }
            Some(engine)
        } else {
            ephemeral = DdEngine::new(Duration::from_secs(60));
            ephemeral.ingest(DdFacts {
                import_edges: edges.clone(),
            })?;
            Some(&ephemeral)
        }
    } else {
        None
    };

    let changed_ids: std::collections::HashSet<i64> = changed_paths
        .iter()
        .filter_map(|p| id_map.get(p.as_str()).copied())
        .collect();

    if let Some(engine) = dd {
        let mut resolver = ConstraintResolver::new();
        let pairs = resolver.resolve(&all_constraints, db, &path_map)?;

        if !pairs.is_empty() {
            engine.set_forbidden_pairs(pairs)?;
            let current_violations = engine.query_violations()?;
            // Compute delta: temporarily remove outgoing edges from changed
            // files to identify which violations are [introduced] by the diff.
            // Only outgoing edges are removed — incoming edges to changed files
            // are controlled by their source, so boundary violations caused by
            // component membership changes won't be labeled [introduced].
            let changed_edges: Vec<(i64, i64)> = edges
                .iter()
                .filter(|(src, _)| changed_ids.contains(src))
                .copied()
                .collect();

            let baseline_set: std::collections::HashSet<(i64, i64)> =
                if !changed_edges.is_empty() {
                    engine.update(DdDelta {
                        added_edges: vec![],
                        removed_edges: changed_edges.clone(),
                    })?;
                    let baseline_result = engine.query_violations();
                    // Re-add edges before propagating any query error
                    engine.update(DdDelta {
                        added_edges: changed_edges,
                        removed_edges: vec![],
                    })?;
                    baseline_result?.into_iter().collect()
                } else {
                    current_violations.iter().copied().collect()
                };

            let current_set: std::collections::HashSet<(i64, i64)> =
                current_violations.iter().copied().collect();

            for &(from_id, to_id) in &current_violations {
                let from_path = path_map.get(&from_id).cloned().unwrap_or_default();
                let to_path = path_map.get(&to_id).cloned().unwrap_or_default();
                if !changed_ids.contains(&from_id) && !changed_ids.contains(&to_id) {
                    continue;
                }
                let is_introduced = !baseline_set.contains(&(from_id, to_id));
                if let Some(c) = constraints::find_matching_constraint(
                    &all_constraints,
                    &from_path,
                    &to_path,
                    &file_to_component,
                    &comp_name_to_id,
                ) {
                    constraint_violations.push(ConstraintViolation {
                        constraint_id: c.id.clone(),
                        constraint_name: c.name.clone(),
                        constraint_kind: c.kind.kind_tag().to_string(),
                        severity: c.severity.as_str().to_string(),
                        provenance: c.provenance.clone(),
                        from_path: from_path.clone(),
                        to_path: to_path.clone(),
                        component_context: constraints::build_component_context(
                            &c.kind,
                            &file_to_component,
                            &from_path,
                            &to_path,
                        ),
                        detail: constraints::format_violation_detail(c, &from_path, &to_path, is_introduced),
                    });
                }
            }

            for &(from_id, to_id) in &baseline_set {
                if current_set.contains(&(from_id, to_id)) {
                    continue;
                }
                if !changed_ids.contains(&from_id) && !changed_ids.contains(&to_id) {
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
                    resolved_constraint_violations.push(ConstraintViolation {
                        constraint_id: c.id.clone(),
                        constraint_name: c.name.clone(),
                        constraint_kind: c.kind.kind_tag().to_string(),
                        severity: c.severity.as_str().to_string(),
                        provenance: c.provenance.clone(),
                        from_path: from_path.clone(),
                        to_path: to_path.clone(),
                        component_context: constraints::build_component_context(
                            &c.kind,
                            &file_to_component,
                            &from_path,
                            &to_path,
                        ),
                        detail: constraints::format_violation_detail(c, &from_path, &to_path, false),
                    });
                }
            }
        }

        if !changed_ids.is_empty() {
            for cycle in engine.query_cycles()? {
                if cycle.file_ids.iter().any(|id| changed_ids.contains(id)) {
                    let cycle_paths: Vec<String> = cycle
                        .file_ids
                        .iter()
                        .filter_map(|id| path_map.get(id).cloned())
                        .collect();
                    let matched = match_no_cycles_constraint(&all_constraints, &cycle_paths);
                    constraint_violations.push(ConstraintViolation {
                        constraint_id: matched
                            .map(|c| c.id.clone())
                            .unwrap_or_else(|| "builtin:cycles".into()),
                        constraint_name: matched.and_then(|c| c.name.clone()),
                        constraint_kind: "no_cycles".into(),
                        severity: matched
                            .map(|c| c.severity.as_str().to_string())
                            .unwrap_or_else(|| "blocking".into()),
                        provenance: matched.and_then(|c| c.provenance.clone()),
                        from_path: cycle_paths.first().cloned().unwrap_or_default(),
                        to_path: cycle_paths.last().cloned().unwrap_or_default(),
                        component_context: None,
                        detail: format!("import cycle: {}", cycle_paths.join(" -> ")),
                    });
                }
            }
        }

        constraint_violations_total = constraint_violations.len();
    }

    // Constraint waiver partition (parallel to convention waiver partition below)
    let constraint_waivers = db.get_constraint_waivers(None)?;
    let mut waived_constraint_violations = Vec::new();
    let mut unwaived_constraints = Vec::new();
    for v in constraint_violations {
        let waiver = constraint_waivers.iter().find(|w| {
            w.constraint_id == v.constraint_id
                && w.file_path == v.from_path
                && w.symbol_qualified_name.is_none()
        });
        if let Some(w) = waiver {
            waived_constraint_violations.push(WaivedConstraintViolation {
                constraint_id: v.constraint_id,
                constraint_name: v.constraint_name,
                constraint_kind: v.constraint_kind,
                severity: v.severity,
                provenance: v.provenance,
                from_path: v.from_path,
                to_path: v.to_path,
                component_context: v.component_context,
                detail: v.detail,
                rationale: w.rationale.clone(),
                waived_by: w.waived_by.clone(),
            });
        } else {
            unwaived_constraints.push(v);
        }
    }
    let constraint_violations = unwaived_constraints;

    // FCA: convention violations on changed symbols
    let mut convention_violations = Vec::new();
    let mut convention_matches = Vec::new();
    let mut drift_alerts = Vec::new();
    let mut convention_drift_findings: Vec<crate::health::HealthFinding> = Vec::new();

    let mut all_sym_attrs = Vec::new();
    let mut sig_info_map: HashMap<String, conventions::templates::SymbolSignatureInfo> =
        HashMap::new();
    for f in &all_files {
        let syms = db.find_symbols_by_file(f.id)?;
        let refs = db.find_refs_in_file(f.id)?;

        let target_ids: Vec<i64> = refs
            .iter()
            .filter(|r| r.context_kind == "call")
            .filter_map(|r| r.target_symbol_id)
            .collect();
        let mut callee_cache: HashMap<i64, conventions::ResolvedCallee> = HashMap::new();
        for id in &target_ids {
            if !callee_cache.contains_key(id) {
                if let Some(sym) = db.symbol_by_id(*id)? {
                    callee_cache.insert(*id, conventions::ResolvedCallee {
                        qualified_name: sym.qualified_name,
                        signature: sym.signature,
                    });
                }
            }
        }

        for s in &syms {
            if let Some(mut attrs) = conventions::extract_attrs_for_symbol(&s, &f.path, &f.language, registry) {
                if let Some(adapter) = registry.adapter_for_language(&f.language) {
                    if let Some(fca_source) = adapter.as_fca_source() {
                        let call_refs: Vec<_> = refs
                            .iter()
                            .filter(|r| {
                                r.context_kind == "call"
                                    && r.line >= s.start_line
                                    && r.line <= s.end_line
                            })
                            .collect();
                        conventions::enrich_with_effects(
                            &mut attrs,
                            s,
                            &call_refs,
                            &|id| callee_cache.get(&id).map(|c| conventions::ResolvedCallee {
                                qualified_name: c.qualified_name.clone(),
                                signature: c.signature.clone(),
                            }),
                            fca_source.effect_patterns(),
                        );
                    }
                }
                sig_info_map.insert(
                    s.qualified_name.clone(),
                    conventions::templates::SymbolSignatureInfo {
                        signature: s.signature.clone(),
                        visibility: s.visibility.clone(),
                        language_attrs: s.language_attrs.clone(),
                        cognitive: s.cognitive,
                    },
                );
                all_sym_attrs.push(attrs);
            }
        }
    }

    if !all_sym_attrs.is_empty() {
        for sa in &mut all_sym_attrs {
            sa.component_id = file_to_component.get(&sa.file).cloned();
        }

        let mut global_engine = FcaEngine::new();
        let global_conventions = global_engine.rebuild(&all_sym_attrs);

        let mut all_convs = global_conventions.clone();
        let mut comp_symbol_groups: Vec<(String, String, Vec<conventions::SymbolAttrs>)> =
            Vec::new();
        for (comp_id, name, paths) in &comp_with_paths {
            let path_set: std::collections::HashSet<&str> =
                paths.iter().map(|p| p.as_str()).collect();
            let comp_symbols: Vec<_> = all_sym_attrs
                .iter()
                .filter(|s| path_set.contains(s.file.as_str()))
                .cloned()
                .collect();
            if comp_symbols.len() < 2 {
                continue;
            }
            let min_support = conventions::component_min_support(comp_symbols.len());
            let mut comp_engine = FcaEngine::new();
            let comp_convs = comp_engine.rebuild_with_params(
                &comp_symbols,
                min_support,
                conventions::MIN_CONFIDENCE,
                Some(comp_id),
            );
            let deduped =
                conventions::deduplicate_component_conventions(comp_convs, &global_conventions);
            all_convs.extend(deduped);
            comp_symbol_groups.push((comp_id.clone(), name.clone(), comp_symbols));
        }

        let fca_conformance =
            crate::health::drift::compute_fca_conformance(&all_convs, &comp_symbol_groups);
        let hrr_coherence =
            crate::health::drift::compute_hrr_coherence(db).unwrap_or_default();

        convention_drift_findings = crate::health::drift::detect_convention_drift(
            db,
            &fca_conformance,
            &hrr_coherence,
            &comp_symbol_groups,
            &file_to_component,
            &id_map,
        )
        .unwrap_or_default();

        drift_alerts = conventions::drift::record_and_detect_drift(
            db,
            &comp_symbol_groups,
            &fca_conformance,
            &hrr_coherence,
        )
        .unwrap_or_default();

        for c in &all_convs {
            let _ = db.upsert_convention(
                &c.id,
                &c.antecedent.join(", "),
                &c.consequent.join(", "),
                c.support as i64,
                c.confidence,
                c.component_id.as_deref(),
            );
        }
        let current_ids: Vec<&str> = all_convs.iter().map(|c| c.id.as_str()).collect();

        let snapshot_id = uuid::Uuid::new_v4().to_string();
        for c in &all_convs {
            let _ = db.record_convention_history(
                &c.id, c.support as i64, c.confidence, &snapshot_id,
            );
        }
        if let Ok(absent) = db.tracked_convention_ids_absent_from(&current_ids) {
            for cid in &absent {
                let _ = db.record_convention_history(cid, 0, 0.0, &snapshot_id);
            }
        }

        if let Ok(signals) = conventions::lifecycle::detect_signals(db) {
            let _ = conventions::lifecycle::generate_proposals(db, signals);
        }

        let _ = db.delete_stale_conventions(&current_ids);

        if let Err(e) = conventions::templates::generate_templates_for_conventions(
            &all_convs, &all_sym_attrs, &sig_info_map, db,
        ) {
            tracing::warn!("template generation failed: {e}");
        }
        if let Err(e) = db.delete_orphan_templates(&current_ids) {
            tracing::warn!("orphan template cleanup failed: {e}");
        }

        let changed_set: std::collections::HashSet<&str> =
            changed_paths.iter().map(|p| p.as_str()).collect();
        let changed_sym_attrs: Vec<_> = all_sym_attrs
            .iter()
            .filter(|a| changed_set.contains(a.file.as_str()))
            .cloned()
            .collect();

        let db_forbidden: Vec<String> = db
            .all_conventions_merged()?
            .iter()
            .filter(|c| c.lifecycle_state.as_deref() == Some("forbidden"))
            .map(|c| c.id.clone())
            .collect();
        let mut effective_conventions = rules.conventions.clone();
        for id in db_forbidden {
            if !effective_conventions.suppress.contains(&id) {
                effective_conventions.suppress.push(id);
            }
        }

        let mut check_engine = FcaEngine::new();
        check_engine.set_conventions(all_convs);

        for v in check_engine.check(&changed_sym_attrs, &effective_conventions) {
            convention_violations.push(ConventionViolation {
                symbol: v.symbol,
                file: v.file,
                convention_id: v.convention_id,
                antecedent: v.antecedent,
                consequent: v.consequent,
                missing: v.missing,
                support: v.support,
                confidence: v.confidence,
            });
        }

        let merged = db.all_conventions_merged()?;
        let deprecated_ids: std::collections::HashSet<String> = merged.iter()
            .filter(|c| c.lifecycle_state.as_deref() == Some("deprecated"))
            .map(|c| c.id.clone()).collect();
        let forbidden_ids: std::collections::HashSet<String> = merged.iter()
            .filter(|c| c.lifecycle_state.as_deref() == Some("forbidden"))
            .map(|c| c.id.clone()).collect();

        if !deprecated_ids.is_empty() || !forbidden_ids.is_empty() {
            for m in check_engine.check_inverse(&changed_sym_attrs, &deprecated_ids, &forbidden_ids) {
                convention_matches.push(ConventionMatchFinding {
                    symbol: m.symbol,
                    file: m.file,
                    convention_id: m.convention_id,
                    antecedent: m.antecedent,
                    consequent: m.consequent,
                    lifecycle_state: m.lifecycle_state,
                    support: m.support,
                    confidence: m.confidence,
                });
            }
        }
    }

    let waivers = db.waivers_for_check()?;
    let sym_component: HashMap<(&str, &str), &str> = all_sym_attrs
        .iter()
        .filter_map(|s| s.component_id.as_deref().map(|c| ((s.file.as_str(), s.name.as_str()), c)))
        .collect();
    let mut waived_violations = Vec::new();
    let mut unwaived = Vec::new();
    for v in convention_violations {
        let comp = sym_component
            .get(&(v.file.as_str(), v.symbol.as_str()))
            .copied()
            .unwrap_or("");
        let file_qualified = format!("{}::{}", v.file, v.symbol);
        let rationale = [
            (v.convention_id.as_str(), v.symbol.as_str(), comp),
            (v.convention_id.as_str(), file_qualified.as_str(), comp),
            (v.convention_id.as_str(), v.symbol.as_str(), ""),
            (v.convention_id.as_str(), file_qualified.as_str(), ""),
        ]
        .iter()
        .find_map(|(cid, sym, cmp)| {
            waivers.get(&(cid.to_string(), sym.to_string(), cmp.to_string()))
        })
        .cloned();
        if let Some(rationale) = rationale {
            waived_violations.push(WaivedViolation {
                symbol: v.symbol,
                file: v.file,
                convention_id: v.convention_id,
                antecedent: v.antecedent,
                consequent: v.consequent,
                missing: v.missing,
                support: v.support,
                confidence: v.confidence,
                rationale,
            });
        } else {
            unwaived.push(v);
        }
    }

    Ok(ReviewFindings {
        constraint_violations,
        resolved_constraint_violations,
        waived_constraint_violations,
        constraint_violations_total,
        convention_violations: unwaived,
        convention_matches,
        waived_violations,
        drift_alerts,
        convention_drift_findings,
    })
}

struct ChangeStats {
    changed_files: Vec<serde_json::Value>,
    changed_symbols: Vec<serde_json::Value>,
    symbol_ids: Vec<i64>,
    total_blast: i64,
    max_cognitive: i64,
    total_churn: u32,
    hotspot_files: u32,
}

fn gather_change_stats(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    churn: &ChurnMap,
) -> Result<ChangeStats> {
    let mut stats = ChangeStats {
        changed_files: Vec::new(),
        changed_symbols: Vec::new(),
        symbol_ids: Vec::new(),
        total_blast: 0,
        max_cognitive: 0,
        total_churn: 0,
        hotspot_files: 0,
    };

    for path in changed_paths {
        let file_churn = churn.counts.get(path).copied().unwrap_or(0);
        stats.total_churn += file_churn;

        if let Some(file) = db.file_by_path(path)? {
            let fl: FreshnessLevel =
                freshness::check_file(workspace_root, path, &file.last_parsed).into();
            stats.total_blast += file.blast_radius;
            if file_churn > 5 && file.blast_radius > 10 {
                stats.hotspot_files += 1;
            }
            let syms = db.find_symbols_by_file(file.id)?;
            for s in &syms {
                stats.symbol_ids.push(s.id);
                let cog = s.cognitive.unwrap_or(0);
                if cog > stats.max_cognitive {
                    stats.max_cognitive = cog;
                }
                stats.changed_symbols.push(json!({
                    "symbol": s.qualified_name, "file": path, "cognitive": cog,
                }));
            }
            stats.changed_files.push(json!({
                "path": path, "blast_radius": file.blast_radius, "symbol_count": syms.len(),
                "_freshness": fl,
            }));
        } else {
            stats.changed_files.push(json!({
                "path": path, "blast_radius": 0, "symbol_count": 0,
                "_freshness": FreshnessLevel::StaleIndex,
            }));
        }
    }
    Ok(stats)
}

fn gather_affected(
    db: &Db,
    symbol_ids: &[i64],
    changed_paths: &[String],
) -> Result<(Vec<(String, i64)>, Vec<(String, String, i64, i64)>)> {
    let affected_file_ids = if symbol_ids.is_empty() {
        Vec::new()
    } else {
        db.find_files_referencing_symbols(symbol_ids)?
    };

    let changed_set: std::collections::HashSet<&str> =
        changed_paths.iter().map(|p| p.as_str()).collect();

    let mut files = Vec::new();
    let mut symbols = Vec::new();

    for fid in &affected_file_ids {
        if let Some(file) = db.file_by_id(*fid)? {
            if changed_set.contains(file.path.as_str()) {
                continue;
            }
            for s in db.find_symbols_by_file(file.id)? {
                symbols.push((
                    s.qualified_name.clone(),
                    file.path.clone(),
                    file.blast_radius,
                    s.cognitive.unwrap_or(0),
                ));
            }
            files.push((file.path.clone(), file.blast_radius));
        }
    }

    files.sort_by(|a, b| a.0.cmp(&b.0));
    files.dedup_by(|a, b| a.0 == b.0);
    files.sort_by(|a, b| b.1.cmp(&a.1));

    symbols.sort_by(|a, b| a.0.cmp(&b.0));
    symbols.dedup_by(|a, b| a.0 == b.0);
    symbols.sort_by(|a, b| b.2.cmp(&a.2));

    Ok((files, symbols))
}

fn file_freshness(db: &Db, workspace_root: &Path, path: &str) -> FreshnessLevel {
    db.file_by_path(path)
        .ok()
        .flatten()
        .map(|f| freshness::check_file(workspace_root, path, &f.last_parsed).into())
        .unwrap_or(FreshnessLevel::StaleIndex)
}

fn behavioral_coupling(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
) -> Vec<serde_json::Value> {
    let config = match components::load_config(workspace_root) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let threshold = config.cochange_threshold.unwrap_or(0.5);

    let mut changed_ids: HashMap<i64, &str> = HashMap::new();
    for p in changed_paths {
        if let Ok(Some(f)) = db.file_by_path(p) {
            changed_ids.insert(f.id, p.as_str());
        }
    }
    if changed_ids.is_empty() {
        return Vec::new();
    }

    let cochange_pairs = match db.cochange_pairs_above_threshold(threshold) {
        Ok(pairs) => pairs,
        Err(_) => return Vec::new(),
    };

    let all_files: HashMap<i64, String> = db
        .all_files()
        .unwrap_or_default()
        .into_iter()
        .map(|f| (f.id, f.path))
        .collect();

    let static_edges: HashSet<(i64, i64)> = db
        .static_file_edges()
        .unwrap_or_default()
        .into_iter()
        .collect();

    let mut entries: Vec<(f64, serde_json::Value)> = cochange_pairs
        .into_iter()
        .filter_map(|(fa, fb, jaccard, shared)| {
            let (changed_id, partner_id) = if changed_ids.contains_key(&fa) && !changed_ids.contains_key(&fb) {
                (fa, fb)
            } else if changed_ids.contains_key(&fb) && !changed_ids.contains_key(&fa) {
                (fb, fa)
            } else {
                return None;
            };
            if static_edges.contains(&(changed_id.min(partner_id), changed_id.max(partner_id))) {
                return None;
            }
            let changed_path = changed_ids.get(&changed_id)?;
            let partner_path = all_files.get(&partner_id)?;
            if components::is_test_file(changed_path) != components::is_test_file(partner_path) {
                return None;
            }
            Some((jaccard, json!({
                "changed_file": changed_path,
                "partner": partner_path,
                "jaccard": jaccard,
                "shared_commits": shared,
            })))
        })
        .collect();

    entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    entries.into_iter().map(|(_, v)| v).collect()
}

fn build_recommended_reads(
    db: &Db,
    workspace_root: &Path,
    findings: &ReviewFindings,
    affected_files: &[(String, i64)],
    changed_files: &[serde_json::Value],
    behavioral_partners: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut violation_sites: Vec<(String, i64, f64)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for v in &findings.convention_violations {
        if !seen.insert(v.file.clone()) {
            continue;
        }
        let blast = affected_files
            .iter()
            .find(|(p, _)| p == &v.file)
            .map(|(_, b)| *b)
            .or_else(|| {
                changed_files
                    .iter()
                    .find(|cf| cf["path"].as_str() == Some(v.file.as_str()))
                    .and_then(|cf| cf["blast_radius"].as_i64())
            })
            .unwrap_or(0);
        violation_sites.push((v.file.clone(), blast, v.confidence));
    }
    violation_sites.sort_by(|a, b| {
        b.2.partial_cmp(&a.2)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut reads: Vec<(String, i64, bool, bool)> = Vec::new();
    for (path, blast, _) in &violation_sites {
        reads.push((path.clone(), *blast, true, false));
    }
    for bp in behavioral_partners {
        if let Some(partner) = bp["partner"].as_str() {
            if seen.insert(partner.to_string()) {
                let blast = db
                    .file_by_path(partner)
                    .ok()
                    .flatten()
                    .map(|f| f.blast_radius)
                    .unwrap_or(0);
                reads.push((partner.to_string(), blast, false, true));
            }
        }
    }
    for (path, blast) in affected_files {
        if !seen.contains(path) {
            reads.push((path.clone(), *blast, false, false));
        }
    }
    reads.truncate(MAX_READS);

    reads.iter().map(|(path, blast, is_violation, is_behavioral)| {
        let fl = file_freshness(db, workspace_root, path);
        let mut entry = json!({ "path": path, "blast_radius": blast, "violation_site": is_violation, "_freshness": fl });
        if *is_behavioral {
            entry["behavioral_partner"] = json!(true);
        }
        entry
    }).collect()
}

pub fn compute(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    churn: &ChurnMap,
    findings: &ReviewFindings,
) -> Result<serde_json::Value> {
    if changed_paths.is_empty() {
        return Ok(json!({
            "changed_files": [],
            "changed_symbols": [],
            "affected_files": [],
            "affected_symbols": [],
            "affected_total": { "files": 0, "symbols": 0 },
            "risk_score": 0.0,
            "risk_breakdown": {
                "blast_radius": 0.0, "complexity_delta": 0.0,
                "hotspot_overlap": 0.0, "churn": 0.0, "convention_violations": 0.0,
            },
            "recommended_reads": [],
            "constraint_violations": [],
            "resolved_constraint_violations": [],
            "constraint_violations_total": 0,
            "waived_constraint_violations": [],
            "convention_violations": [],
            "convention_matches": [],
            "waived_violations": [],
            "drift_alerts": [],
            "convention_drift": [],
        }));
    }

    let stats = gather_change_stats(db, workspace_root, changed_paths, churn)?;
    let (affected_files, affected_symbols) = gather_affected(db, &stats.symbol_ids, changed_paths)?;

    let total_affected_files = affected_files.len();
    let total_affected_symbols = affected_symbols.len();

    let affected_files_out: Vec<_> = affected_files
        .iter()
        .take(MAX_AFFECTED)
        .map(|(path, blast)| {
            let fl = file_freshness(db, workspace_root, path);
            json!({ "path": path, "blast_radius": blast, "_freshness": fl })
        })
        .collect();
    let affected_symbols_out: Vec<_> = affected_symbols.iter().take(MAX_AFFECTED)
        .map(|(sym, file, blast, cog)| {
            let fl = file_freshness(db, workspace_root, file);
            json!({ "symbol": sym, "file": file, "blast_radius": blast, "cognitive": cog, "_freshness": fl })
        }).collect();

    let constraint_violations_out: Vec<_> = findings
        .constraint_violations
        .iter()
        .map(|v| {
            json!({
                "constraint_id": v.constraint_id,
                "constraint_name": v.constraint_name,
                "kind": v.constraint_kind,
                "severity": v.severity,
                "provenance": v.provenance,
                "from": v.from_path,
                "to": v.to_path,
                "component_context": v.component_context,
                "detail": v.detail,
            })
        })
        .collect();
    let resolved_constraint_violations_out: Vec<_> = findings
        .resolved_constraint_violations
        .iter()
        .map(|v| {
            json!({
                "constraint_id": v.constraint_id,
                "constraint_name": v.constraint_name,
                "kind": v.constraint_kind,
                "severity": v.severity,
                "provenance": v.provenance,
                "from": v.from_path,
                "to": v.to_path,
                "component_context": v.component_context,
                "detail": v.detail,
            })
        })
        .collect();
    let waived_constraint_violations_out: Vec<_> = findings
        .waived_constraint_violations
        .iter()
        .map(|v| {
            json!({
                "constraint_id": v.constraint_id,
                "constraint_name": v.constraint_name,
                "kind": v.constraint_kind,
                "severity": v.severity,
                "from": v.from_path,
                "to": v.to_path,
                "component_context": v.component_context,
                "detail": v.detail,
                "waived": true,
                "rationale": v.rationale,
                "waived_by": v.waived_by,
            })
        })
        .collect();
    let convention_violations_out: Vec<_> = findings
        .convention_violations
        .iter()
        .map(|v| {
            json!({
                "symbol": v.symbol, "file": v.file, "convention_id": v.convention_id,
                "antecedent": v.antecedent, "consequent": v.consequent, "missing": v.missing,
                "support": v.support, "confidence": v.confidence,
            })
        })
        .collect();
    let convention_matches_out: Vec<_> = findings
        .convention_matches
        .iter()
        .map(|m| {
            let severity = if m.lifecycle_state == "forbidden" { "warning" } else { "advisory" };
            json!({
                "symbol": m.symbol, "file": m.file, "convention_id": m.convention_id,
                "antecedent": m.antecedent, "consequent": m.consequent,
                "lifecycle_state": m.lifecycle_state, "severity": severity,
                "support": m.support, "confidence": m.confidence,
            })
        })
        .collect();
    let waived_violations_out: Vec<_> = findings
        .waived_violations
        .iter()
        .map(|w| {
            json!({
                "symbol": w.symbol, "file": w.file, "convention_id": w.convention_id,
                "antecedent": w.antecedent, "consequent": w.consequent, "missing": w.missing,
                "support": w.support, "confidence": w.confidence,
                "waived": true, "rationale": w.rationale,
            })
        })
        .collect();
    let drift_alerts_out: Vec<_> = findings
        .drift_alerts
        .iter()
        .map(|a| {
            let diverging: Vec<_> = a
                .diverging_attributes
                .iter()
                .map(|d| {
                    json!({
                        "attribute": d.attribute,
                        "old_proportion": scoring::round3(d.old_proportion),
                        "new_proportion": scoring::round3(d.new_proportion),
                    })
                })
                .collect();
            json!({
                "component_id": a.component_id,
                "component_name": a.component_name,
                "entropy_old": scoring::round3(a.entropy_old),
                "entropy_new": scoring::round3(a.entropy_new),
                "delta": scoring::round3(a.delta),
                "diverging_attributes": diverging,
            })
        })
        .collect();
    let convention_drift_out: Vec<_> = findings
        .convention_drift_findings
        .iter()
        .map(|f| {
            json!({
                "biomarker": f.biomarker_kind.as_str(),
                "severity": f.severity.as_str(),
                "file_id": f.file_id,
                "metric_value": scoring::round3(f.metric_value),
                "threshold": scoring::round3(f.threshold),
                "provenance": &f.provenance,
                "detail": &f.detail,
            })
        })
        .collect();

    let file_count = changed_paths.len();
    let blast_score = scoring::normalize(stats.total_blast as f64, scoring::BLAST_NORM);
    let complexity_score = scoring::normalize(stats.max_cognitive as f64, scoring::COMPLEXITY_NORM);
    let hotspot_score =
        scoring::normalize(stats.hotspot_files as f64, (file_count as f64).max(1.0));
    let churn_score = scoring::normalize(stats.total_churn as f64, scoring::CHURN_NORM);
    let convention_score = scoring::normalize(findings.convention_violations.len() as f64, 5.0);

    let risk_score = scoring::weighted_score(&[
        Signal {
            weight: W_BLAST,
            score: blast_score,
        },
        Signal {
            weight: W_COMPLEXITY,
            score: complexity_score,
        },
        Signal {
            weight: W_HOTSPOT,
            score: hotspot_score,
        },
        Signal {
            weight: W_CHURN,
            score: churn_score,
        },
        Signal {
            weight: W_CONVENTIONS,
            score: convention_score,
        },
    ]);

    let behavioral = behavioral_coupling(db, workspace_root, changed_paths);
    let recommended_reads = build_recommended_reads(
        db,
        workspace_root,
        findings,
        &affected_files,
        &stats.changed_files,
        &behavioral,
    );

    let mut result = json!({
        "changed_files": stats.changed_files,
        "changed_symbols": stats.changed_symbols,
        "affected_files": affected_files_out,
        "affected_symbols": affected_symbols_out,
        "affected_total": {
            "files": total_affected_files,
            "files_truncated": total_affected_files > MAX_AFFECTED,
            "symbols": total_affected_symbols,
            "symbols_truncated": total_affected_symbols > MAX_AFFECTED,
        },
        "risk_score": scoring::round3(risk_score),
        "risk_breakdown": {
            "blast_radius": scoring::round3(blast_score),
            "complexity_delta": scoring::round3(complexity_score),
            "hotspot_overlap": scoring::round3(hotspot_score),
            "churn": scoring::round3(churn_score),
            "convention_violations": scoring::round3(convention_score),
        },
        "constraint_violations": constraint_violations_out,
        "resolved_constraint_violations": resolved_constraint_violations_out,
        "constraint_violations_total": findings.constraint_violations_total,
        "waived_constraint_violations": waived_constraint_violations_out,
        "convention_violations": convention_violations_out,
        "convention_matches": convention_matches_out,
        "waived_violations": waived_violations_out,
        "drift_alerts": drift_alerts_out,
        "convention_drift": convention_drift_out,
        "recommended_reads": recommended_reads,
    });
    if !behavioral.is_empty() {
        result["behavioral_coupling"] = json!(behavioral);
    }
    Ok(result)
}

