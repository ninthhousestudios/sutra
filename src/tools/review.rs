use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::components;
use crate::constraints::DdEngine;
use crate::constraints::check::{self, ContentSource, DiffImportEdges, EvalScope, FactsSource};
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
    #[serde(default)]
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

// Renormalized after removing the deviations factor (sutra/313): weights
// previously summed to 1.0 including deviations (weight 0.2); scaling the
// remaining four by 1/0.8 preserves that invariant.
const W_BLAST: f64 = 0.375;
const W_COMPLEXITY: f64 = 0.25;
const W_HOTSPOT: f64 = 0.1875;
const W_CHURN: f64 = 0.1875;

pub use crate::constraints::ConstraintFinding;
use crate::waivers::Waived;

#[derive(Default)]
pub struct ReviewFindings {
    pub constraint_violations: Vec<ConstraintFinding>,
    pub resolved_constraint_violations: Vec<ConstraintFinding>,
    pub waived_constraint_violations: Vec<Waived<ConstraintFinding>>,
    pub constraint_parse_errors: Vec<rules::ConstraintParseError>,
    pub constraint_violations_total: usize,
    /// Report-only instance acks (sutra/305) on changed files, as JSON. Surfaced
    /// so acknowledged clones dropped from `constraint_violations` stay visible on
    /// the review surface, not silent (sutra/306) — parity with waivers.
    pub acknowledged: Vec<serde_json::Value>,
    /// Operator-facing warnings from resolving `.sutra/accepted.toml` against the
    /// live rules (unknown/ambiguous constraint refs). Surfaced so a waiver
    /// pointing at a deleted constraint is visible, not silently inert
    /// (sutra/308 hazard 4).
    pub accepted_warnings: Vec<String>,
}

pub fn handle(
    db: &Db,
    workspace_root: &Path,
    diff_mode: Option<&str>,
    dd_engine: Option<&DdEngine>,
    explain: bool,
) -> Result<serde_json::Value> {
    let mode = diff_mode.unwrap_or("branch");

    let scope = resolve_diff_entries(workspace_root, mode)?;
    let changed_paths = scope.paths();
    let (base_revision, head_revision) = (&scope.base_revision, &scope.head_revision);

    let churn = ChurnMap {
        counts: git::git_churn(workspace_root, change_signals::CHURN_WINDOW_DAYS)?,
        window_days: change_signals::CHURN_WINDOW_DAYS,
    };

    let registry = crate::parser::adapter::default_registry();
    let (findings, findings_error) = match build_findings(
        db,
        workspace_root,
        &changed_paths,
        base_revision,
        // The compositor assesses current state — read the working tree, not the
        // diff snapshot, regardless of `diff_mode` (sutra/385).
        ContentSource::Worktree,
        dd_engine,
        &registry,
    ) {
        Ok(f) => (f, None),
        Err(e) => (ReviewFindings::default(), Some(e.to_string())),
    };

    let shape_config = crate::similarity::diff::ShapeChangeConfig::default();
    let shape_changes = crate::similarity::diff::detect_shape_changes(
        db,
        workspace_root,
        &changed_paths,
        base_revision,
        head_revision.as_deref(),
        &registry,
        &shape_config,
    );

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
    }
    Ok(result)
}

/// A resolved diff: the changed files (renames carry `old_path`) and the two
/// revisions they are compared between.
pub struct DiffScope {
    pub entries: Vec<git::DiffFileEntry>,
    /// `""` (the index) for unstaged, `HEAD` for staged, an explicit revision
    /// otherwise. Read with `git show {base}:{path}`, so `""` resolves to the index.
    pub base_revision: String,
    /// `Some("")` for staged (the index), `None` for unstaged (the worktree), an
    /// explicit revision otherwise.
    pub head_revision: Option<String>,
}

impl DiffScope {
    pub fn paths(&self) -> Vec<String> {
        self.entries.iter().map(|e| e.path.to_string()).collect()
    }
}

/// Resolve a diff-mode string to `(changed_paths, base_revision, head_revision)`.
///
/// Shared by the review compositor and the `sutra check` CLI gate so both
/// interpret `"staged"` / `"unstaged"` / `"branch"` / a commit spec identically.
/// `head_revision` is `Some("")` for staged (the index), `None` for unstaged
/// (the worktree), and an explicit revision otherwise; `base_revision` is `""`
/// (the index) for unstaged, so both sides match what `git diff` compared.
pub fn resolve_diff_scope(
    workspace_root: &Path,
    mode: &str,
) -> Result<(Vec<String>, String, Option<String>)> {
    let scope = resolve_diff_entries(workspace_root, mode)?;
    let paths = scope.paths();
    Ok((paths, scope.base_revision, scope.head_revision))
}

/// [`resolve_diff_scope`] keeping the per-file entries, so a rename's base side
/// is read from its old path. Every mode detects renames (`--name-status -M`).
pub fn resolve_diff_entries(workspace_root: &Path, mode: &str) -> Result<DiffScope> {
    let (entries, base_revision, head_revision) = match mode {
        "staged" => (
            git::git_diff_staged_entries(workspace_root)?,
            "HEAD".to_string(),
            Some(String::new()),
        ),
        // `git diff` compares the index to the worktree, so the base side is
        // the index too: reading HEAD would leak staged changes into an
        // unstaged review (sutra/458).
        "unstaged" => (
            git::git_diff_unstaged_entries(workspace_root)?,
            String::new(),
            None,
        ),
        "branch" => {
            let default_branch = git::detect_default_branch(workspace_root)?;
            let base = git::git_merge_base(workspace_root, &default_branch)?;
            let entries = git::git_diff_files(workspace_root, &base, "HEAD")?;
            (entries, base, Some("HEAD".to_string()))
        }
        spec => {
            let (base, head) = if let Some((a, b)) = spec.split_once("..") {
                (a.to_string(), b.to_string())
            } else {
                (format!("{spec}~1"), spec.to_string())
            };
            let entries = git::git_diff_files(workspace_root, &base, &head)?;
            (entries, base, Some(head))
        }
    };
    Ok(DiffScope {
        entries,
        base_revision,
        head_revision,
    })
}

fn extract_outgoing_edges(
    content: &str,
    rel_path: &str,
    file_id: i64,
    workspace_root: &Path,
    id_map: &HashMap<&str, i64>,
) -> Option<Vec<(i64, i64)>> {
    let language = if rel_path.ends_with(".rs") {
        "rust"
    } else if rel_path.ends_with(".dart") {
        "dart"
    } else {
        return None;
    };
    let result = match crate::parser::parse_file(content, language, rel_path) {
        Ok(r) if r.parsed_ok => r,
        _ => return None,
    };
    // Review keeps test imports: its base edges are diffed against the index's
    // full edge set (`import_edges`, test edges included), and per-constraint
    // `include_tests` is applied downstream. The guard drops them (sutra/290).
    crate::import_edges::content_import_edges(
        workspace_root,
        rel_path,
        file_id,
        language,
        &result,
        id_map,
        crate::import_edges::TestImports::Keep,
    )
}

pub fn build_findings(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
    base_revision: &str,
    // Where forbidden-pattern content is read for the scoped files. `sutra check`
    // passes the requested snapshot (the staged index or a commit); the review
    // compositor passes `Worktree` to preserve its assess-current-state contract.
    content: ContentSource,
    shared_dd: Option<&DdEngine>,
    registry: &LanguageRegistry,
) -> Result<ReviewFindings> {
    let _rules = rules::load_rules(workspace_root)?;
    let all_files = db.all_files()?;
    let id_map: HashMap<&str, i64> = all_files.iter().map(|f| (&*f.path, f.id)).collect();

    // Constraint evaluation via unified check core
    let changed_ids: HashSet<i64> = changed_paths
        .iter()
        .filter_map(|p| id_map.get(p.as_str()).copied())
        .collect();

    let mut old_edges: HashSet<(i64, i64)> = HashSet::new();
    let mut import_delta = DiffImportEdges::default();
    for path in changed_paths {
        let file_id = match id_map.get(path.as_str()) {
            Some(&id) => id,
            None => continue,
        };
        let old_content = match git::git_file_content_at(workspace_root, base_revision, path) {
            Ok(Some(c)) => c,
            Ok(None) => {
                import_delta.added_ids.insert(file_id);
                continue;
            }
            Err(_) => continue,
        };
        let Some(base_edges) =
            extract_outgoing_edges(&old_content, path, file_id, workspace_root, &id_map)
        else {
            continue;
        };
        old_edges.extend(base_edges.iter().copied());
        // Fan-in attribution compares like with like: the same extractor over
        // the requested snapshot, and only when both sides extracted (sutra/440).
        if let Some(head_content) = check::read_scoped_content(workspace_root, content, path)
            && let Some(head_edges) =
                extract_outgoing_edges(&head_content, path, file_id, workspace_root, &id_map)
        {
            import_delta.base_edges.extend(base_edges);
            import_delta.head_edges.extend(head_edges);
        }
    }

    // Changed stubs have no file row, so they never make it into changed_ids.
    let changed_pattern_only_paths: Vec<String> = changed_paths
        .iter()
        .filter(|p| crate::constraints::patterns::is_pattern_only_path(p, registry))
        .cloned()
        .collect();

    let changed_set: HashSet<&str> = changed_paths.iter().map(|p| p.as_str()).collect();

    let check_outcome = check::evaluate(
        &FactsSource::DdBacked {
            db,
            dd_engine: shared_dd,
        },
        workspace_root,
        EvalScope::ChangedFiles {
            changed_ids: &changed_ids,
            old_edges: &old_edges,
            import_delta: &import_delta,
            changed_pattern_only_paths: &changed_pattern_only_paths,
            content,
            changed_paths: &changed_set,
        },
        registry,
    )?;

    let constraint_violations = check_outcome.active;
    let resolved_constraint_violations = check_outcome.resolved;
    let waived_constraint_violations = check_outcome.waived;
    let constraint_violations_total =
        constraint_violations.len() + waived_constraint_violations.len();
    let constraint_parse_errors = check_outcome.parse_errors;
    let accepted_warnings = check_outcome.accepted_warnings;

    // Report-only instance acks on the changed files, so acknowledged clones
    // dropped from constraint_violations stay visible here (sutra/306).
    let acknowledged = crate::tools::constraints::acked_instances_json(db, Some(&changed_set))?;

    Ok(ReviewFindings {
        constraint_violations,
        resolved_constraint_violations,
        waived_constraint_violations,
        constraint_parse_errors,
        constraint_violations_total,
        acknowledged,
        accepted_warnings,
    })
}

fn file_freshness(db: &Db, workspace_root: &Path, path: &str) -> FreshnessLevel {
    db.file_by_path(path)
        .ok()
        .flatten()
        .map(|f| freshness::check_file(workspace_root, path, &f.last_parsed).into())
        .unwrap_or(FreshnessLevel::StaleIndex)
}

/// A review partner must have co-changed in at least this many commits. One
/// shared commit at jaccard >= 0.5 means both files were barely touched: born in
/// the same commit, or swept by one small sync drop. Across four repos about 6
/// of 81 such pairs were worth a look (sutra/476). Entity co-change uses the
/// same floor.
const MIN_PARTNER_SHARED_COMMITS: i64 = 2;

/// Co-change partners of the changed files that share no static edge with them.
/// A failure is an `Err`, never an empty list: "no partners" must mean the
/// history was read and none qualified (sutra/476).
fn behavioral_coupling(
    db: &Db,
    workspace_root: &Path,
    changed_paths: &[String],
) -> Result<Vec<serde_json::Value>> {
    let config = components::load_config(workspace_root)?;
    let threshold = config
        .cochange_threshold
        .unwrap_or(components::DEFAULT_COCHANGE_THRESHOLD);

    let mut changed_ids: HashMap<i64, &str> = HashMap::new();
    for p in changed_paths {
        if let Some(f) = db.file_by_path(p)? {
            changed_ids.insert(f.id, p.as_str());
        }
    }
    if changed_ids.is_empty() {
        return Ok(Vec::new());
    }

    let cochange_pairs = db.cochange_pairs_above_threshold(threshold)?;

    let all_files: HashMap<i64, Arc<str>> = db
        .all_files()?
        .into_iter()
        .map(|f| (f.id, f.path))
        .collect();

    let static_edges: HashSet<(i64, i64)> = db.static_file_edges()?.into_iter().collect();

    let mut entries: Vec<(f64, serde_json::Value)> = cochange_pairs
        .into_iter()
        .filter_map(|(fa, fb, jaccard, shared)| {
            if shared < MIN_PARTNER_SHARED_COMMITS {
                return None;
            }
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
    Ok(entries.into_iter().map(|(_, v)| v).collect())
}

fn build_recommended_reads(
    db: &Db,
    workspace_root: &Path,
    affected_files: &[change_signals::AffectedFile],
    behavioral_partners: &[serde_json::Value],
) -> Vec<serde_json::Value> {
    let mut seen = std::collections::HashSet::new();
    let mut reads: Vec<(String, i64, bool)> = Vec::new();
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
            reads.push((partner.to_string(), blast, true));
        }
    }
    for a in affected_files {
        if !seen.contains(&a.path) {
            reads.push((a.path.clone(), a.blast_radius, false));
        }
    }
    reads.truncate(MAX_READS);

    reads
        .iter()
        .map(|(path, blast, is_behavioral)| {
            let fl = file_freshness(db, workspace_root, path);
            let mut entry = json!({ "path": path, "blast_radius": blast, "_freshness": fl });
            if *is_behavioral {
                entry["behavioral_partner"] = json!(true);
            }
            entry
        })
        .collect()
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
                "hotspot_overlap": 0.0, "churn": 0.0,
            },
            "recommended_reads": [],
            "constraint_violations": [],
            "resolved_constraint_violations": [],
            "constraint_violations_total": 0,
            "waived_constraint_violations": [],
        });
        if explain {
            result["_explain"] = json!({
                "formula": "sum(weight_i * min(raw_i / ceiling_i, 1.0)), clamped to [0, 1]",
                "weights": {
                    "blast_radius": { "weight": W_BLAST, "ceiling": change_signals::BLAST_NORM, "contribution": 0.0, "rationale": "blast radius of changed symbols" },
                    "complexity": { "weight": W_COMPLEXITY, "ceiling": change_signals::COMPLEXITY_NORM, "contribution": 0.0, "rationale": "peak cognitive complexity in changed code" },
                    "hotspot_overlap": { "weight": W_HOTSPOT, "ceiling": 1.0, "contribution": 0.0, "rationale": "proportion of changed files that are churn hotspots" },
                    "churn": { "weight": W_CHURN, "ceiling": change_signals::CHURN_NORM, "contribution": 0.0, "rationale": "total recent churn across changed files" },
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
            let mut entry = json!({
                "constraint_id": v.constraint_id,
                "constraint_name": v.constraint_name,
                "kind": v.constraint_kind,
                "severity": v.severity.as_str(),
                "provenance": v.provenance,
                "from": v.from_path,
                "to": v.to_path,
                "component_context": v.component_context,
                "detail": v.detail,
            });
            if let Some(line) = v.line {
                entry["line"] = json!(line);
            }
            if let Some(snippet) = &v.snippet {
                entry["snippet"] = json!(snippet);
            }
            if let Some(sym) = &v.enclosing_symbol {
                entry["enclosing_symbol"] = json!(sym);
            }
            entry
        })
        .collect();
    let resolved_constraint_violations_out: Vec<_> = findings
        .resolved_constraint_violations
        .iter()
        .map(|v| {
            let mut entry = json!({
                "constraint_id": v.constraint_id,
                "constraint_name": v.constraint_name,
                "kind": v.constraint_kind,
                "severity": v.severity.as_str(),
                "provenance": v.provenance,
                "from": v.from_path,
                "to": v.to_path,
                "component_context": v.component_context,
                "detail": v.detail,
            });
            if let Some(line) = v.line {
                entry["line"] = json!(line);
            }
            if let Some(snippet) = &v.snippet {
                entry["snippet"] = json!(snippet);
            }
            if let Some(sym) = &v.enclosing_symbol {
                entry["enclosing_symbol"] = json!(sym);
            }
            entry
        })
        .collect();
    let waived_constraint_violations_out: Vec<_> = findings
        .waived_constraint_violations
        .iter()
        .map(|v| {
            let mut entry = json!({
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
            });
            if let Some(line) = v.finding.line {
                entry["line"] = json!(line);
            }
            if let Some(snippet) = &v.finding.snippet {
                entry["snippet"] = json!(snippet);
            }
            if let Some(sym) = &v.finding.enclosing_symbol {
                entry["enclosing_symbol"] = json!(sym);
            }
            entry
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
    ]);

    let (behavioral, behavioral_error) =
        match behavioral_coupling(db, workspace_root, changed_paths) {
            Ok(entries) => (entries, None),
            Err(e) => (Vec::new(), Some(e.to_string())),
        };
    let recommended_reads =
        build_recommended_reads(db, workspace_root, &signals.affected_files, &behavioral);

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
        },
        "constraint_violations": constraint_violations_out,
        "resolved_constraint_violations": resolved_constraint_violations_out,
        "constraint_violations_total": findings.constraint_violations_total,
        "waived_constraint_violations": waived_constraint_violations_out,
        "recommended_reads": recommended_reads,
    });
    if !behavioral.is_empty() {
        result["behavioral_coupling"] = json!(behavioral);
    }
    if let Some(err) = behavioral_error {
        result["behavioral_coupling_error"] = json!(err);
    }
    if !findings.acknowledged.is_empty() {
        result["acknowledged"] = json!(findings.acknowledged);
    }
    if !findings.accepted_warnings.is_empty() {
        result["accepted_warnings"] = json!(findings.accepted_warnings);
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
            },
        });
    }
    Ok(result)
}
