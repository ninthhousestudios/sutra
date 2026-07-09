use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::components;
use crate::constraints::DdEngine;
use crate::constraints::check::{self, EvalScope, FactsSource};
use crate::conventions;
use crate::db::Db;
use crate::error::Result;
use crate::freshness::{self, FreshnessLevel};
use crate::git;
use crate::parser::adapter::LanguageRegistry;
use crate::rules;
use crate::tools::change_signals::{self, ChurnMap};
use crate::tools::scoring::{self, Signal};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReviewArgs {
    pub workspace: String,
    /// "branch" (default), "staged", "unstaged", or a commit spec (e.g. "HEAD~3..HEAD", "abc123")
    #[serde(default)]
    pub diff: Option<String>,
    /// When true, include `_explain` with weights, ceilings, and per-signal contributions.
    #[serde(default)]
    pub explain: Option<bool>,
}

const MAX_AFFECTED: usize = 20;
const MAX_READS: usize = 10;

const W_BLAST: f64 = 0.30;
const W_COMPLEXITY: f64 = 0.20;
const W_HOTSPOT: f64 = 0.15;
const W_CHURN: f64 = 0.15;
const W_CONVENTIONS: f64 = 0.20;

pub use crate::constraints::ConstraintFinding;
pub use crate::conventions::Deviation;
use crate::waivers::Waived;

#[derive(Default)]
pub struct ReviewFindings {
    pub constraint_violations: Vec<ConstraintFinding>,
    pub resolved_constraint_violations: Vec<ConstraintFinding>,
    pub waived_constraint_violations: Vec<Waived<ConstraintFinding>>,
    pub constraint_parse_errors: Vec<rules::ConstraintParseError>,
    pub constraint_violations_total: usize,
    pub deviations: Vec<Deviation>,
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    diff_mode: Option<&str>,
    dd_engine: Option<&DdEngine>,
    explain: bool,
) -> Result<serde_json::Value> {
    let mode = diff_mode.unwrap_or("branch");

    let (changed_paths, base_revision, head_revision) = match mode {
        "staged" => (
            git::git_diff_staged(workspace_root)?,
            "HEAD".to_string(),
            Some(String::new()),
        ),
        "unstaged" => (
            git::git_diff_unstaged(workspace_root)?,
            "HEAD".to_string(),
            None,
        ),
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
        counts: git::git_churn(workspace_root, change_signals::CHURN_WINDOW_DAYS)?,
        window_days: change_signals::CHURN_WINDOW_DAYS,
    };

    let registry = crate::parser::adapter::default_registry();
    let (findings, findings_error) = match build_findings(
        db,
        workspace_root,
        &changed_paths,
        &base_revision,
        dd_engine,
        &registry,
    ) {
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

    let mut result = compute(
        db,
        workspace_root,
        &changed_paths,
        &churn,
        &findings,
        explain,
    )?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("diff_mode".into(), json!(mode));
        obj.insert(
            "churn_window_days".into(),
            json!(change_signals::CHURN_WINDOW_DAYS),
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

fn extract_outgoing_edges(
    content: &str,
    rel_path: &str,
    file_id: i64,
    workspace_root: &Path,
    id_map: &HashMap<&str, i64>,
) -> Vec<(i64, i64)> {
    let language = if rel_path.ends_with(".rs") {
        "rust"
    } else if rel_path.ends_with(".dart") {
        "dart"
    } else {
        return Vec::new();
    };
    let result = match crate::parser::parse_file(content, language, rel_path) {
        Ok(r) if r.parsed_ok => r,
        _ => return Vec::new(),
    };
    let mut edges = Vec::new();
    match language {
        "rust" => {
            let layout = crate::rust_imports::parse_workspace_layout(workspace_root);
            let path_ref_map: HashMap<&str, i64> = id_map.iter().map(|(k, v)| (*k, *v)).collect();
            for import in &result.imports {
                let resolved = match crate::rust_imports::normalize_to_crate_segments(
                    &import.raw_path,
                    rel_path,
                    &layout,
                ) {
                    Some(r) if !r.segments.is_empty() => r,
                    _ => continue,
                };
                if let Some(target_id) = crate::rust_imports::resolve_segments(
                    &resolved.segments,
                    &path_ref_map,
                    &resolved.src_prefix,
                ) {
                    if target_id != file_id {
                        edges.push((file_id, target_id));
                    }
                }
            }
        }
        "dart" => {
            let pkg_map = crate::dart_packages::DartPackageMap::build(workspace_root);
            let id_to_path: HashMap<i64, &str> = id_map.iter().map(|(k, v)| (*v, *k)).collect();
            for import in &result.imports {
                let resolved = if import.raw_path.starts_with("package:") {
                    crate::dart_packages::resolve_package_uri(&import.raw_path, &pkg_map)
                } else if import.raw_path.ends_with(".dart")
                    && !import.raw_path.starts_with("dart:")
                {
                    crate::dart_packages::resolve_relative_import(
                        &import.raw_path,
                        file_id,
                        &id_to_path,
                    )
                } else {
                    None
                };
                if let Some(path) = resolved {
                    if let Some(&target_id) = id_map.get(path.as_str()) {
                        if target_id != file_id {
                            edges.push((file_id, target_id));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    edges
}

pub fn build_findings(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    base_revision: &str,
    shared_dd: Option<&DdEngine>,
    registry: &LanguageRegistry,
) -> Result<ReviewFindings> {
    let _rules = rules::load_rules(workspace_root)?;
    let all_files = db.all_files()?;
    let id_map: HashMap<&str, i64> = all_files.iter().map(|f| (&*f.path, f.id)).collect();

    let comp_with_paths = db.active_components_with_paths()?;
    let mut file_to_component: HashMap<&str, &str> = HashMap::new();
    for (comp_id, _, paths) in &comp_with_paths {
        for path in paths {
            file_to_component.insert(path, comp_id);
        }
    }

    // Constraint evaluation via unified check core
    let changed_ids: HashSet<i64> = changed_paths
        .iter()
        .filter_map(|p| id_map.get(p.as_str()).copied())
        .collect();

    let mut old_edges: HashSet<(i64, i64)> = HashSet::new();
    for path in changed_paths {
        let file_id = match id_map.get(path.as_str()) {
            Some(&id) => id,
            None => continue,
        };
        if let Ok(Some(old_content)) = git::git_file_content_at(workspace_root, base_revision, path)
        {
            for edge in extract_outgoing_edges(&old_content, path, file_id, workspace_root, &id_map)
            {
                old_edges.insert(edge);
            }
        }
    }

    let check_outcome = check::evaluate(
        &FactsSource::DdBacked {
            db,
            dd_engine: shared_dd,
        },
        workspace_root,
        EvalScope::ChangedFiles {
            changed_ids: &changed_ids,
            old_edges: &old_edges,
        },
    )?;

    let constraint_violations = check_outcome.active;
    let resolved_constraint_violations = check_outcome.resolved;
    let waived_constraint_violations = check_outcome.waived;
    let constraint_violations_total =
        constraint_violations.len() + waived_constraint_violations.len();
    let constraint_parse_errors = check_outcome.parse_errors;

    // Deviation detection: on-the-fly FCA over component siblings
    let changed_set: HashSet<&str> = changed_paths.iter().map(|p| p.as_str()).collect();
    let mut changed_sym_attrs: Vec<conventions::SymbolAttrs> = Vec::new();
    let mut all_sym_attrs_by_component: HashMap<String, Vec<conventions::SymbolAttrs>> =
        HashMap::new();
    let mut orphan_sym_attrs: Vec<conventions::SymbolAttrs> = Vec::new();

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

        let is_changed = changed_set.contains(&*f.path);
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
                attrs.component_id = file_to_component
                    .get(attrs.file.as_str())
                    .map(|s| s.to_string());

                if is_changed {
                    changed_sym_attrs.push(attrs.clone());
                }

                match &attrs.component_id {
                    Some(cid) => all_sym_attrs_by_component
                        .entry(cid.clone())
                        .or_default()
                        .push(attrs),
                    None => orphan_sym_attrs.push(attrs),
                }
            }
        }
    }

    let sketch_components: HashSet<String> = comp_with_paths
        .iter()
        .filter(|(id, _, _)| db.component_lifecycle_state(id).ok().as_deref() == Some("sketch"))
        .map(|(id, _, _)| id.clone())
        .collect();

    let toolchain_pairs = collect_toolchain_pairs(registry);
    let deviations = conventions::detect_deviations(
        &changed_sym_attrs,
        &all_sym_attrs_by_component,
        &orphan_sym_attrs,
        &toolchain_pairs,
        &sketch_components,
    );

    Ok(ReviewFindings {
        constraint_violations,
        resolved_constraint_violations,
        waived_constraint_violations,
        constraint_parse_errors,
        constraint_violations_total,
        deviations,
    })
}

pub(crate) fn collect_toolchain_pairs(
    registry: &LanguageRegistry,
) -> Vec<crate::parser::adapter::ToolchainPair> {
    let mut pairs = Vec::new();
    for lang in &["rust", "dart", "python", "c"] {
        if let Some(adapter) = registry.adapter_for_language(lang)
            && let Some(fca_source) = adapter.as_fca_source()
        {
            pairs.extend_from_slice(fca_source.toolchain_enforced_pairs());
        }
    }
    pairs
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

    let all_files: HashMap<i64, Arc<str>> = db
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
            let (changed_id, partner_id) =
                if changed_ids.contains_key(&fa) && !changed_ids.contains_key(&fb) {
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
            Some((
                jaccard,
                json!({
                    "changed_file": changed_path,
                    "partner": partner_path,
                    "jaccard": jaccard,
                    "shared_commits": shared,
                }),
            ))
        })
        .collect();

    entries.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    entries.into_iter().map(|(_, v)| v).collect()
}

fn build_recommended_reads(
    db: &Db,
    workspace_root: &Path,
    findings: &ReviewFindings,
    affected_files: &[change_signals::AffectedFile],
    changed_files: &[serde_json::Value],
    behavioral_partners: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut violation_sites: Vec<(String, i64, f64)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for d in &findings.deviations {
        if d.informational {
            continue;
        }
        if !seen.insert(d.file.clone()) {
            continue;
        }
        let blast = affected_files
            .iter()
            .find(|a| a.path == d.file)
            .map(|a| a.blast_radius)
            .or_else(|| {
                changed_files
                    .iter()
                    .find(|cf| cf["path"].as_str() == Some(d.file.as_str()))
                    .and_then(|cf| cf["blast_radius"].as_i64())
            })
            .unwrap_or(0);
        violation_sites.push((d.file.clone(), blast, d.confidence));
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
        if let Some(partner) = bp["partner"].as_str()
            && seen.insert(partner.to_string())
        {
            let blast = db
                .file_by_path(partner)
                .ok()
                .flatten()
                .map(|f| f.blast_radius)
                .unwrap_or(0);
            reads.push((partner.to_string(), blast, false, true));
        }
    }
    for a in affected_files {
        if !seen.contains(&a.path) {
            reads.push((a.path.clone(), a.blast_radius, false, false));
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
    explain: bool,
) -> Result<serde_json::Value> {
    if changed_paths.is_empty() {
        let mut result = json!({
            "changed_files": [],
            "changed_symbols": [],
            "affected_files": [],
            "affected_symbols": [],
            "affected_total": { "files": 0, "symbols": 0 },
            "risk_score": 0.0,
            "risk_breakdown": {
                "blast_radius": 0.0, "complexity_delta": 0.0,
                "hotspot_overlap": 0.0, "churn": 0.0, "deviations": 0.0,
            },
            "recommended_reads": [],
            "constraint_violations": [],
            "resolved_constraint_violations": [],
            "constraint_violations_total": 0,
            "waived_constraint_violations": [],
            "deviations": [],
        });
        if explain {
            result["_explain"] = json!({
                "formula": "sum(weight_i * min(raw_i / ceiling_i, 1.0)), clamped to [0, 1]",
                "weights": {
                    "blast_radius": { "weight": W_BLAST, "ceiling": change_signals::BLAST_NORM, "contribution": 0.0, "rationale": "blast radius of changed symbols" },
                    "complexity": { "weight": W_COMPLEXITY, "ceiling": change_signals::COMPLEXITY_NORM, "contribution": 0.0, "rationale": "peak cognitive complexity in changed code" },
                    "hotspot_overlap": { "weight": W_HOTSPOT, "ceiling": 1.0, "contribution": 0.0, "rationale": "proportion of changed files that are churn hotspots" },
                    "churn": { "weight": W_CHURN, "ceiling": change_signals::CHURN_NORM, "contribution": 0.0, "rationale": "total recent churn across changed files" },
                    "deviations": { "weight": W_CONVENTIONS, "ceiling": 5.0, "contribution": 0.0, "rationale": "number of pattern deviations detected" },
                },
            });
        }
        return Ok(result);
    }

    let signals = change_signals::gather(db, changed_paths, churn, true)?;

    let changed_files_out: Vec<_> = signals
        .per_file
        .iter()
        .map(|f| {
            let fl = file_freshness(db, workspace_root, &f.path);
            json!({
                "path": f.path, "blast_radius": f.blast_radius,
                "symbol_count": f.symbols.len(), "_freshness": fl,
            })
        })
        .collect();
    let changed_symbols_out: Vec<_> = signals
        .per_file
        .iter()
        .flat_map(|f| {
            f.symbols.iter().map(|s| {
                json!({
                    "symbol": s.qualified_name, "file": f.path, "cognitive": s.cognitive,
                })
            })
        })
        .collect();

    let total_affected_files = signals.affected_files.len();
    let total_affected_symbols = signals.affected_symbols.len();

    let affected_files_out: Vec<_> = signals
        .affected_files
        .iter()
        .take(MAX_AFFECTED)
        .map(|a| {
            let fl = file_freshness(db, workspace_root, &a.path);
            json!({ "path": a.path, "blast_radius": a.blast_radius, "_freshness": fl })
        })
        .collect();
    let affected_symbols_out: Vec<_> = signals
        .affected_symbols
        .iter()
        .take(MAX_AFFECTED)
        .map(|a| {
            let fl = file_freshness(db, workspace_root, &a.file);
            json!({ "symbol": a.qualified_name, "file": a.file, "blast_radius": a.blast_radius, "cognitive": a.cognitive, "_freshness": fl })
        })
        .collect();

    let constraint_violations_out: Vec<_> = findings
        .constraint_violations
        .iter()
        .map(|v| {
            json!({
                "constraint_id": v.constraint_id,
                "constraint_name": v.constraint_name,
                "kind": v.constraint_kind,
                "severity": v.severity.as_str(),
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
                "severity": v.severity.as_str(),
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
                "constraint_id": v.finding.constraint_id,
                "constraint_name": v.finding.constraint_name,
                "kind": v.finding.constraint_kind,
                "severity": v.finding.severity.as_str(),
                "from": v.finding.from_path,
                "to": v.finding.to_path,
                "component_context": v.finding.component_context,
                "detail": v.finding.detail,
                "waived": true,
                "rationale": v.rationale,
                "waived_by": v.waived_by,
            })
        })
        .collect();
    let deviations_out: Vec<_> = findings
        .deviations
        .iter()
        .map(|d| {
            json!({
                "symbol": d.symbol,
                "file": d.file,
                "pattern": format!("{} → {}",
                    d.pattern_antecedent.join(", "),
                    d.pattern_consequent.join(", ")),
                "missing": d.missing,
                "evidence": format!("{}/{} siblings have {}",
                    d.conforming, d.total_matching,
                    d.pattern_consequent.join(", ")),
                "exemplars": d.exemplars,
                "support": d.support,
                "confidence": d.confidence,
                "strength": scoring::round3(d.strength),
                "informational": d.informational,
            })
        })
        .collect();
    let file_count = changed_paths.len();
    let blast_score = scoring::normalize(signals.total_blast as f64, change_signals::BLAST_NORM);
    let complexity_score = scoring::normalize(
        signals.max_cognitive.unwrap_or(0) as f64,
        change_signals::COMPLEXITY_NORM,
    );
    let hotspot_ceiling = (file_count as f64).max(1.0);
    let hotspot_score = scoring::normalize(signals.hotspot_files as f64, hotspot_ceiling);
    let churn_score = scoring::normalize(signals.total_churn as f64, change_signals::CHURN_NORM);
    let actionable_deviations = findings
        .deviations
        .iter()
        .filter(|d| !d.informational)
        .count();
    let deviation_score = scoring::normalize(actionable_deviations as f64, 5.0);

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
            score: deviation_score,
        },
    ]);

    let behavioral = behavioral_coupling(db, workspace_root, changed_paths);
    let recommended_reads = build_recommended_reads(
        db,
        workspace_root,
        findings,
        &signals.affected_files,
        &changed_files_out,
        &behavioral,
    );

    let mut result = json!({
        "changed_files": changed_files_out,
        "changed_symbols": changed_symbols_out,
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
            "deviations": scoring::round3(deviation_score),
        },
        "constraint_violations": constraint_violations_out,
        "resolved_constraint_violations": resolved_constraint_violations_out,
        "constraint_violations_total": findings.constraint_violations_total,
        "waived_constraint_violations": waived_constraint_violations_out,
        "deviations": deviations_out,
        "recommended_reads": recommended_reads,
    });
    if !behavioral.is_empty() {
        result["behavioral_coupling"] = json!(behavioral);
    }
    if !findings.constraint_parse_errors.is_empty() {
        result["constraint_parse_errors"] = json!(
            findings
                .constraint_parse_errors
                .iter()
                .map(|e| json!({
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
                }))
                .collect::<Vec<_>>()
        );
    }
    if explain {
        result["_explain"] = json!({
            "formula": "sum(weight_i * min(raw_i / ceiling_i, 1.0)), clamped to [0, 1]",
            "weights": {
                "blast_radius": { "weight": W_BLAST, "ceiling": change_signals::BLAST_NORM, "contribution": scoring::round3(W_BLAST * blast_score), "rationale": "blast radius of changed symbols" },
                "complexity": { "weight": W_COMPLEXITY, "ceiling": change_signals::COMPLEXITY_NORM, "contribution": scoring::round3(W_COMPLEXITY * complexity_score), "rationale": "peak cognitive complexity in changed code" },
                "hotspot_overlap": { "weight": W_HOTSPOT, "ceiling": hotspot_ceiling, "contribution": scoring::round3(W_HOTSPOT * hotspot_score), "rationale": "proportion of changed files that are churn hotspots" },
                "churn": { "weight": W_CHURN, "ceiling": change_signals::CHURN_NORM, "contribution": scoring::round3(W_CHURN * churn_score), "rationale": "total recent churn across changed files" },
                "deviations": { "weight": W_CONVENTIONS, "ceiling": 5.0, "contribution": scoring::round3(W_CONVENTIONS * deviation_score), "rationale": "number of pattern deviations detected" },
            },
        });
    }
    Ok(result)
}
