use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use glob::{MatchOptions, Pattern};

use crate::constraints::{self, ConstraintResolver, DdEngine, accepted, external};
pub use crate::constraints::{ConstraintFinding, FindingDelta};
use crate::db::{ConstraintRatchetRow, active_ratchets_from_conn};
use crate::error::Result;
use crate::parser::adapter::LanguageRegistry;
use crate::rules::{
    self, Constraint, ConstraintKind, ConstraintParseError, Severity, match_no_cycles_constraint,
};
use crate::waivers::{self, Waived};

#[derive(Debug, Clone, Default)]
pub struct CheckOutcome {
    pub active: Vec<ConstraintFinding>,
    pub waived: Vec<Waived<ConstraintFinding>>,
    pub resolved: Vec<ConstraintFinding>,
    pub parse_errors: Vec<ConstraintParseError>,
    /// Operator-facing warnings from resolving `.sutra/accepted.toml` against the
    /// live rule set (unknown/ambiguous constraint refs). Surfaced on the report
    /// so a waiver silently pointing at a deleted constraint is visible, never
    /// dropped (sutra/308 hazard 4). Only the DD-backed report path populates
    /// this; the guard's `RawConn` path leaves it empty (config warnings belong
    /// on the report, not at the edit-time gate).
    pub accepted_warnings: Vec<String>,
}

pub enum EvalScope<'a> {
    Workspace,
    ChangedFiles {
        changed_ids: &'a HashSet<i64>,
        old_edges: &'a HashSet<(i64, i64)>,
        /// Changed files that are pattern-eligible but unindexed (`.pyi` stubs).
        /// They have no id, so they can't ride along in `changed_ids` — without
        /// them a changed stub is invisible to review-scoped pattern checks.
        changed_pattern_only_paths: &'a [String],
    },
    SingleFile(i64),
    Edges {
        edges: &'a [(i64, i64)],
        /// Proposed `(from_path, crate_name, is_test)` external imports
        /// (guard side).
        externals: &'a [(String, String, bool)],
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
    registry: &LanguageRegistry,
) -> Result<CheckOutcome> {
    match facts {
        // A DD-backed evaluation reads files (→ path_map), components and import
        // edges across separate `Mutex<Connection>` acquisitions. A reparse
        // remints file ids and commits per file, and the constraint/orient
        // endpoints don't hold the parse lock — so a reparse landing mid-read
        // can leave path_map on old ids while the edges the engine syncs to
        // carry new ones, reviving the disjoint-id silent-clean of sutra/297.
        // Guard the read window with the index's data_generation: if it moved
        // while we read, the snapshot was incoherent, so retry against a fresh
        // one. Sustained churn (a long reparse) surfaces as an explicit error
        // rather than a silently-empty result (sutra/298).
        FactsSource::DdBacked { db, dd_engine } => {
            const MAX_ATTEMPTS: u32 = 4;
            for _ in 0..MAX_ATTEMPTS {
                let gen_before = db.get_data_generation()?;
                let outcome = evaluate_dd(db, *dd_engine, workspace_root, &scope, registry)?;
                if db.get_data_generation()? == gen_before {
                    return Ok(outcome);
                }
            }
            Err(crate::error::SutraError::Internal(
                "constraint evaluation could not read a coherent index snapshot: \
                 a reparse kept committing across every attempt"
                    .to_string(),
            ))
        }
        FactsSource::RawConn(conn) => evaluate_raw(conn, workspace_root, scope, registry),
    }
}

/// Ids of the constraints that aim themselves at test code, so path-based test
/// exclusion must step aside for them (sutra/296). Borrowed from the slice: the
/// set lives no longer than the constraints it describes.
fn test_directed_ids(constraints: &[Constraint]) -> HashSet<&str> {
    constraints
        .iter()
        .filter(|c| {
            constraints::constraint_targets_tests(
                c,
                &crate::parser::adapter::any_language_is_test_path,
            )
        })
        .map(|c| c.id.as_ref())
        .collect()
}

/// Subtract report-only instance acks from `forbidden_pattern` active findings
/// (sutra/305). Acks are honored ONLY here on the report path — never by the
/// guard — so a specific examined clone can be acknowledged while its siblings
/// and any future addition stay governed. Count-aware via the shared
/// [`super::patterns::subtract_multiset`]: an ack of N cancels N matches of that
/// content key in that file, and a surplus (e.g. a new byte-identical clone)
/// still surfaces. Non-pattern findings pass through untouched.
///
/// Only reachable from the DD-backed (`evaluate_dd`) report path; the guard's
/// `RawConn` evaluation never calls it, which is what keeps acks off the
/// edit-time surface.
fn apply_instance_acks(
    db: &crate::db::Db,
    active: Vec<ConstraintFinding>,
) -> Result<Vec<ConstraintFinding>> {
    use super::patterns::{self, MatchKey};

    let (mut pattern_findings, mut kept): (Vec<_>, Vec<_>) = active
        .into_iter()
        .partition(|f| f.constraint_kind.as_str() == "forbidden_pattern");
    if pattern_findings.is_empty() {
        return Ok(kept);
    }

    // Acks are stored per file, so cancel per file: an ack in file A must not
    // cancel a same-content match in file B. Sort by path and walk contiguous
    // runs — no owned-key clone needed to group.
    pattern_findings.sort_by(|a, b| a.from_path.cmp(&b.from_path));
    let mut remaining = pattern_findings;
    while !remaining.is_empty() {
        let run_end = remaining
            .iter()
            .take_while(|f| f.from_path == remaining[0].from_path)
            .count();
        let rest_after = remaining.split_off(run_end);
        // `remaining` now holds exactly the run of findings for one file.
        let run = std::mem::replace(&mut remaining, rest_after);
        let mut prior: HashMap<MatchKey, usize> = HashMap::new();
        for a in db.get_constraint_instance_acks_for_file(run[0].from_path.as_str())? {
            let count = a.accepted_count.max(0) as usize;
            *prior
                .entry((a.constraint_id, a.enclosing_symbol, a.snippet))
                .or_default() += count;
        }
        kept.extend(patterns::subtract_multiset(run, prior));
    }
    Ok(kept)
}

fn evaluate_dd(
    db: &crate::db::Db,
    dd_engine: Option<&DdEngine>,
    workspace_root: &Path,
    scope: &EvalScope,
    registry: &LanguageRegistry,
) -> Result<CheckOutcome> {
    let mut loaded_rules = rules::load_rules(workspace_root)?;
    let (all_constraints, parse_errors) = loaded_rules.all_constraints();

    // Freshness gate for `.sutra/accepted.toml`: seed the file from any legacy
    // DB-only rows (migrate, gated on file absence) BEFORE re-projecting the
    // cache — the order is load-bearing (sutra/308 hazard 1). Every DB-backed
    // read below (the waiver partition, the ack subtraction) now derives from a
    // cache coherent with the file on disk.
    let accepted_warnings: Vec<String> =
        accepted::refresh_cache(db, workspace_root, &all_constraints)?
            .iter()
            .map(accepted::AcceptedWarning::message)
            .collect();

    let all_files = db.all_files()?;
    let path_map: HashMap<i64, &str> = all_files.iter().map(|f| (f.id, &*f.path)).collect();

    let comp_with_paths = db.active_components_with_paths()?;
    let mut file_to_component: HashMap<&str, &str> = HashMap::new();
    let mut comp_name_to_id: HashMap<&str, &str> = HashMap::new();
    for (comp_id, name, paths) in &comp_with_paths {
        comp_name_to_id.insert(name, comp_id);
        for path in paths {
            file_to_component.insert(path, comp_id);
        }
    }

    let changed_ids = match scope {
        EvalScope::ChangedFiles { changed_ids, .. } => Some(*changed_ids),
        _ => None,
    };

    let mut findings = Vec::new();
    let mut resolved = Vec::new();

    let has_patterns = all_constraints
        .iter()
        .any(|c| matches!(c.kind, ConstraintKind::ForbiddenPattern { .. }));

    // Pattern-eligible but unindexed files (e.g. Python .pyi stubs) live only on
    // disk, so they have to be walked for. Gated on has_patterns — nothing else
    // consumes these paths, and the walk is O(repo files) on every review.
    // Skipped for the per-file/edge scopes, which are driven by a caller-supplied path.
    let stub_paths: Vec<String> =
        if !has_patterns || matches!(scope, EvalScope::SingleFile(_) | EvalScope::Edges { .. }) {
            Vec::new()
        } else {
            constraints::patterns::scan_pattern_only_files(workspace_root, registry)
        };
    let stub_path_refs: Vec<&str> = stub_paths.iter().map(|p| p.as_str()).collect();

    // Dead constraint detection (Workspace and ChangedFiles scopes only)
    if !matches!(scope, EvalScope::SingleFile(_) | EvalScope::Edges { .. }) {
        let paths: Vec<&str> = all_files.iter().map(|f| &*f.path).collect();
        let component_names: Vec<&str> = comp_with_paths
            .iter()
            .map(|(_, name, _)| name.as_str())
            .collect();
        let component_ids: Vec<&str> = comp_with_paths
            .iter()
            .map(|(id, _, _)| id.as_str())
            .collect();
        for c in &all_constraints {
            let coverage = constraints::constraint_coverage(
                c,
                &paths,
                &stub_path_refs,
                &component_names,
                &component_ids,
            );
            let dead = coverage.dead_fields();
            if !dead.is_empty() {
                findings.push(ConstraintFinding {
                    constraint_id: Arc::clone(&c.id),
                    constraint_name: c.name.clone(),
                    constraint_kind: "dead_constraint".into(),
                    severity: Severity::Informational,
                    provenance: c.provenance.clone(),
                    from_path: String::new(),
                    to_path: String::new(),
                    component_context: None,
                    detail: format!(
                        "{} constraint '{}': zero matches on {} — rule is inert",
                        c.kind.kind_tag(),
                        c.name.as_deref().unwrap_or(&c.id),
                        dead.join(", "),
                    ),
                    delta: FindingDelta::Unknown,
                    line: None,
                    snippet: None,
                    enclosing_symbol: None,
                });
            }
        }
    }

    if external::has_external_constraints(&all_constraints) {
        let unresolved = db.unresolved_imports_with_files()?;
        let layout = crate::rust_imports::parse_workspace_layout(workspace_root);
        let crate_names = layout.all_crate_names();
        let crate_name_refs: Vec<&str> = crate_names.to_vec();
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

    // Forbidden pattern checks — read source from disk for scope-matched files.
    // Runs before the edge-empty early return since patterns are per-file, not edge-based.
    if has_patterns {
        let scan_ids: HashSet<i64> = match scope {
            EvalScope::ChangedFiles { changed_ids, .. } => (*changed_ids).clone(),
            EvalScope::SingleFile(id) => std::iter::once(*id).collect(),
            EvalScope::Edges { .. } => HashSet::new(),
            EvalScope::Workspace => all_files.iter().map(|f| f.id).collect(),
        };
        // Stub files have no id, so the scope's id set can't carry them: a
        // workspace audit takes every stub on disk, a review takes the ones the
        // caller reports as changed. Matches the indexed-file contract, where a
        // changed file is scanned whole rather than diffed for introduced matches.
        let scan_stub_paths: &[String] = match scope {
            EvalScope::Workspace => &stub_paths,
            EvalScope::ChangedFiles {
                changed_pattern_only_paths,
                ..
            } => changed_pattern_only_paths,
            _ => &[],
        };
        if !scan_ids.is_empty() || !scan_stub_paths.is_empty() {
            let mut sources: Vec<(String, String)> = Vec::new();
            for f in &all_files {
                if !scan_ids.contains(&f.id) {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(workspace_root.join(&*f.path)) {
                    sources.push((f.path.to_string(), content));
                }
            }
            for path in scan_stub_paths {
                if let Ok(content) = std::fs::read_to_string(workspace_root.join(path)) {
                    sources.push((path.clone(), content));
                }
            }
            let source_refs: Vec<(&str, &str)> = sources
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_str()))
                .collect();
            findings.extend(super::patterns::check_forbidden_patterns(
                &all_constraints,
                &source_refs,
                registry,
            ));
        }
    }

    let edges = db.import_edges()?;
    // Edges a release build can actually see. The DD graph stays whole (blast
    // radius and cycle discovery both want the full picture); test-only edges
    // are filtered out at finding time so `include_tests` stays per-constraint.
    let production_edges = db.production_import_edges()?;
    // Module-declaration edges (`mod child;`) wire the module tree, not real
    // coupling. Cycle detection subtracts them so an idiomatic `mod.rs`
    // re-export module doesn't read as an architectural cycle (sutra/304).
    let module_edges = db.module_declaration_edges()?;
    if edges.is_empty() {
        let constraint_waivers = db.get_constraint_waivers(None)?;
        let (active, waived) = waivers::partition(findings, &constraint_waivers);
        let mut active = apply_instance_acks(db, active)?;
        let ratchets = db.get_active_constraint_ratchets()?;
        active.extend(check_ratchet_violations(&ratchets, &all_constraints));
        return Ok(CheckOutcome {
            active,
            waived,
            resolved,
            parse_errors,
            accepted_warnings,
        });
    }

    let edge_set: HashSet<(i64, i64)> = edges.iter().copied().collect();

    // The shared engine is per-workspace and per-session, so its graph has to be
    // reconciled with the index on every evaluation — not just ingested once.
    // File ids are reminted on reparse, so a cached graph doesn't degrade
    // gracefully: it goes fully disjoint from the freshly resolved forbidden
    // pairs and every DD-backed constraint reports zero (sutra/297).
    let ephemeral;
    let engine: &DdEngine = if let Some(e) = dd_engine {
        e
    } else {
        ephemeral = DdEngine::new(Duration::from_secs(60));
        &ephemeral
    };
    engine.sync_edges(&edges)?;

    let mut resolver = ConstraintResolver::new();
    let pairs = resolver.resolve(&all_constraints, db, &path_map)?;

    // A rule that aims itself at a test path — `forbidden_dep from = "tests/**"`,
    // a `no_cycles` scoped there — must still fire on the test-only edges it was
    // written for. Test exclusion steps aside for it, the same escape hatch
    // forbidden_pattern already honours (sutra/296). Computed once: the
    // classifier is path-only, so it cannot vary per edge.
    let test_directed = test_directed_ids(&all_constraints);

    if !pairs.is_empty() {
        engine.set_forbidden_pairs(pairs)?;
        let current_violations = engine.query_violations()?;

        let (baseline_set, delta_available) = match scope {
            EvalScope::ChangedFiles {
                old_edges,
                changed_ids,
                ..
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

            let from_path = path_map.get(&from_id).copied().unwrap_or("");
            let to_path = path_map.get(&to_id).copied().unwrap_or("");

            if let Some(c) = constraints::find_matching_constraint(
                &all_constraints,
                from_path,
                to_path,
                &file_to_component,
                &comp_name_to_id,
            ) {
                if !c.include_tests
                    && !test_directed.contains(c.id.as_ref())
                    && !production_edges.contains(&(from_id, to_id))
                {
                    continue;
                }
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
                    from_path,
                    to_path,
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
                let from_path = path_map.get(&from_id).copied().unwrap_or("");
                let to_path = path_map.get(&to_id).copied().unwrap_or("");
                if let Some(c) = constraints::find_matching_constraint(
                    &all_constraints,
                    from_path,
                    to_path,
                    &file_to_component,
                    &comp_name_to_id,
                ) {
                    // A pair still present but now test-only was never
                    // reported active, so it has nothing to resolve. A pair
                    // gone from the graph entirely is a genuine resolution and
                    // must still be reported.
                    if !c.include_tests
                        && !test_directed.contains(c.id.as_ref())
                        && edge_set.contains(&(from_id, to_id))
                        && !production_edges.contains(&(from_id, to_id))
                    {
                        continue;
                    }
                    resolved.push(make_finding(
                        c,
                        from_path,
                        to_path,
                        &file_to_component,
                        FindingDelta::Resolved,
                    ));
                }
            }
        }
    }

    // Cycle detection — validate reported SCCs against current edges
    let cycle_filter = changed_ids;
    for cycle in engine.query_cycles()? {
        if let Some(cids) = cycle_filter
            && !cycle.file_ids.iter().any(|id| cids.contains(id))
        {
            continue;
        }
        let cycle_paths: Vec<&str> = cycle
            .file_ids
            .iter()
            .filter_map(|id| path_map.get(id).copied())
            .collect();
        if cycle_paths.len() < cycle.file_ids.len() {
            continue;
        }
        let cycle_node_set: HashSet<i64> = cycle.file_ids.iter().copied().collect();
        let has_backing = cycle.file_ids.iter().all(|&node| {
            let has_out = edge_set
                .iter()
                .any(|&(s, d)| s == node && cycle_node_set.contains(&d));
            let has_in = edge_set
                .iter()
                .any(|&(s, d)| d == node && cycle_node_set.contains(&s));
            has_out && has_in
        });
        if !has_backing {
            continue;
        }
        let matched = match_no_cycles_constraint(&all_constraints, &cycle_paths);

        // Re-run SCC detection over the edge subset that a genuine cycle would
        // need, dropping edges that don't count as architectural coupling:
        //   - `#[cfg(test)]` imports absent from a release build (sutra/290) —
        //     unless the constraint opted into test edges;
        //   - `mod child;` module-tree wiring, which closes an idiomatic
        //     re-export cycle that isn't real coupling (sutra/304).
        // Whatever sub-cycles survive are reported — nothing, when the whole
        // loop was test wiring or module declarations.
        let base_edges =
            if matched.is_some_and(|c| c.include_tests || test_directed.contains(c.id.as_ref())) {
                &edge_set
            } else {
                &production_edges
            };
        let cycle_edges: HashSet<(i64, i64)> =
            base_edges.difference(&module_edges).copied().collect();
        let reported: Vec<Vec<&str>> = super::worker::compute_sccs(&cycle_node_set, &cycle_edges)
            .into_iter()
            // A singleton SCC is a real cycle only when the file imports
            // itself. `has_backing` established a self-edge exists; what remains
            // is whether the surviving edge subset still backs it.
            .filter(|scc| {
                scc.len() > 1
                    || scc
                        .iter()
                        .next()
                        .is_some_and(|&id| cycle_edges.contains(&(id, id)))
            })
            .map(|scc| {
                let mut ids: Vec<i64> = scc.into_iter().collect();
                ids.sort_unstable();
                ids.iter()
                    .filter_map(|id| path_map.get(id).copied())
                    .collect()
            })
            .collect();

        for paths in reported {
            findings.push(ConstraintFinding {
                constraint_id: matched
                    .map(|c| Arc::clone(&c.id))
                    .unwrap_or_else(|| "builtin:cycles".into()),
                constraint_name: matched.and_then(|c| c.name.clone()),
                constraint_kind: "no_cycles".into(),
                // An authored rule keeps its own severity. An *un-owned* cycle —
                // no `no_cycles` rule covers it — falls back to Advisory, not
                // Blocking: Blocking is reserved for cycles you declared you care
                // about, and those carry a name and are already waivable. A
                // rule-less, name-less builtin finding has no suppression lever
                // (sutra/359), so blocking on it strands the operator. It still
                // surfaces as the safety-net signal; it just no longer gates.
                severity: matched.map(|c| c.severity).unwrap_or(Severity::Advisory),
                provenance: matched.and_then(|c| c.provenance.clone()),
                from_path: paths.first().unwrap_or(&"").to_string(),
                to_path: paths.last().unwrap_or(&"").to_string(),
                component_context: None,
                detail: format!("import cycle: {}", paths.join(" -> ")),
                delta: FindingDelta::Unknown,
                line: None,
                snippet: None,
                enclosing_symbol: None,
            });
        }
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
            findings.push(ConstraintFinding {
                constraint_id: Arc::clone(&c.id),
                constraint_name: c.name.clone(),
                constraint_kind: "max_fan_in".into(),
                severity: c.severity,
                provenance: c.provenance.clone(),
                from_path: f.path.to_string(),
                to_path: String::new(),
                component_context: None,
                detail: format!(
                    "fan-in is {}, threshold is {threshold}: {}",
                    f.fan_in_files, f.path,
                ),
                delta: FindingDelta::Unknown,
                line: None,
                snippet: None,
                enclosing_symbol: None,
            });
        }
    }

    let constraint_waivers = db.get_constraint_waivers(None)?;
    let (active, waived) = waivers::partition(findings, &constraint_waivers);
    let mut active = apply_instance_acks(db, active)?;
    let ratchets = db.get_active_constraint_ratchets()?;
    active.extend(check_ratchet_violations(&ratchets, &all_constraints));

    Ok(CheckOutcome {
        active,
        waived,
        resolved,
        parse_errors,
        accepted_warnings,
    })
}

fn evaluate_raw(
    conn: &rusqlite::Connection,
    workspace_root: &Path,
    scope: EvalScope,
    registry: &LanguageRegistry,
) -> Result<CheckOutcome> {
    use rusqlite::params;

    let mut loaded_rules = rules::load_rules(workspace_root)?;
    let (all_constraints, parse_errors) = loaded_rules.all_constraints();

    let parse_error_findings: Vec<ConstraintFinding> = parse_errors
        .iter()
        .map(|e| ConstraintFinding {
            constraint_id: Arc::from(format!("parse-error-{}", e.index)),
            constraint_name: e.name.as_deref().map(Arc::from),
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
            line: None,
            snippet: None,
            enclosing_symbol: None,
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
    let has_patterns = all_constraints
        .iter()
        .any(|c| matches!(c.kind, rules::ConstraintKind::ForbiddenPattern { .. }));
    if !has_forbidden_or_boundary && !has_external && !has_max_fan_in && !has_patterns {
        let mut active = parse_error_findings;
        active.extend(check_ratchet_violations(
            &active_ratchets_from_conn(conn),
            &all_constraints,
        ));
        return Ok(CheckOutcome {
            active,
            parse_errors,
            ..Default::default()
        });
    }

    let mut external_findings: Vec<ConstraintFinding> = Vec::new();
    if has_external {
        let layout = crate::rust_imports::parse_workspace_layout(workspace_root);
        let crate_names = layout.all_crate_names();
        let crate_name_refs: Vec<&str> = crate_names.to_vec();
        if let Err(msg) =
            external::validate_no_external_targeting_members(&all_constraints, &crate_name_refs)
        {
            external_findings.push(external::config_error_finding(&msg));
        }
        match &scope {
            EvalScope::SingleFile(file_id) => {
                let mut stmt = conn.prepare(
                    "SELECT f.path, f.language, i.imported_path, i.is_test FROM imports i \
                     JOIN files f ON f.id = i.file_id \
                     WHERE i.file_id = ?1 AND i.resolved_file_id IS NULL",
                )?;
                let rows: Vec<(String, String, String, bool)> = stmt
                    .query_map(params![file_id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                let items: Vec<(String, String, bool)> = rows
                    .iter()
                    .filter_map(|(path, lang, imp, is_test)| {
                        external::external_crate_of_import(imp, lang, &crate_name_refs)
                            .map(|c| (path.clone(), c, *is_test))
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
                 AND resolved_file_id IS NOT NULL AND is_test = 0",
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

    if edges.is_empty() && external_findings.is_empty() && !has_max_fan_in && !has_patterns {
        let mut active = parse_error_findings;
        active.extend(check_ratchet_violations(
            &active_ratchets_from_conn(conn),
            &all_constraints,
        ));
        return Ok(CheckOutcome {
            active,
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

    let comp_rows = build_component_rows_raw(conn)?;
    let mut file_to_component: HashMap<&str, &str> = HashMap::new();
    let mut comp_name_to_id: HashMap<&str, &str> = HashMap::new();
    for (comp_id, name, paths) in &comp_rows {
        comp_name_to_id.insert(name, comp_id);
        for path in paths {
            file_to_component.insert(path, comp_id);
        }
    }

    use crate::db::ConstraintWaiverRow;

    let single_file_path: Option<String> = match &scope {
        EvalScope::SingleFile(file_id) => conn
            .query_row(
                "SELECT path FROM files WHERE id = ?1",
                params![file_id],
                |row| row.get(0),
            )
            .ok(),
        _ => None,
    };

    let relevant_paths: HashSet<&str> = path_map
        .values()
        .map(|p| p.as_str())
        .chain(external_findings.iter().map(|f| f.from_path.as_str()))
        .chain(single_file_path.as_deref())
        .collect();
    // The guard holds a read-only connection and cannot reproject the cache. When
    // the cache is fresh (a server review already projected the current file) the
    // DB read is the fast path. When it is stale — a hand-edited `accepted.toml`
    // no server pass has seen yet — derive the waivers straight from that same
    // file, so the guard honors exactly what the next audit will (guard must
    // predict the report, sutra/308 hazard 3). Acks are report-only; the guard
    // never needs them.
    let constraint_waivers: Vec<ConstraintWaiverRow> =
        if accepted::is_cache_fresh_conn(conn, workspace_root)? {
            conn.prepare(
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
            .collect()
        } else {
            accepted::resolve_waivers_for_guard(workspace_root, &all_constraints)?
                .into_iter()
                .filter(|w| relevant_paths.contains(w.file_path.as_str()))
                .map(|w| ConstraintWaiverRow {
                    // The file carries no id/timestamps; the guard only matches on
                    // (constraint_id, file, symbol) and reads rationale/by, so the
                    // synthesized display fields are inert here.
                    id: 0,
                    constraint_id: w.constraint_id.into(),
                    constraint_name: w.constraint_name.map(Into::into),
                    file_path: w.file_path,
                    symbol_qualified_name: w.symbol_qualified_name,
                    rationale: w.rationale,
                    waived_by: w.waived_by,
                    created_at: String::new(),
                    updated_at: String::new(),
                })
                .collect()
        };

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

    // MaxFanIn evaluation
    if has_max_fan_in {
        let glob_opts = MatchOptions {
            require_literal_separator: true,
            ..MatchOptions::default()
        };
        let fan_in_targets: Vec<(String, i64)> = match &scope {
            EvalScope::SingleFile(file_id) => conn
                .prepare("SELECT path, fan_in_files FROM files WHERE id = ?1")?
                .query_row(params![file_id], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok()
                .into_iter()
                .collect(),
            EvalScope::Edges { edges, .. } => {
                let target_ids: HashSet<i64> = edges.iter().map(|(_, t)| *t).collect();
                let mut rows = Vec::new();
                let mut stmt =
                    conn.prepare("SELECT path, fan_in_files FROM files WHERE id = ?1")?;
                for tid in &target_ids {
                    if let Ok(row) = stmt.query_row(params![tid], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                    }) {
                        rows.push(row);
                    }
                }
                rows
            }
            _ => Vec::new(),
        };
        for (path, fan_in) in &fan_in_targets {
            for c in &all_constraints {
                let ConstraintKind::MaxFanIn { target, threshold } = &c.kind else {
                    continue;
                };
                if *fan_in <= *threshold as i64 {
                    continue;
                }
                if let Ok(pat) = Pattern::new(target)
                    && pat.matches_with(path, glob_opts)
                {
                    findings.push(ConstraintFinding {
                        constraint_id: Arc::clone(&c.id),
                        constraint_name: c.name.clone(),
                        constraint_kind: "max_fan_in".into(),
                        severity: c.severity,
                        provenance: c.provenance.clone(),
                        from_path: path.clone(),
                        to_path: String::new(),
                        component_context: None,
                        detail: format!("fan-in is {fan_in}, threshold is {threshold}: {path}",),
                        delta: FindingDelta::Unknown,
                        line: None,
                        snippet: None,
                        enclosing_symbol: None,
                    });
                }
            }
        }
    }

    // Forbidden pattern checks — read source from disk for scope-matched files
    if has_patterns && !matches!(scope, EvalScope::Edges { .. }) {
        let scan_paths: Vec<String> = match &scope {
            EvalScope::SingleFile(_) => single_file_path.into_iter().collect(),
            _ => Vec::new(),
        };
        if !scan_paths.is_empty() {
            let mut sources: Vec<(String, String)> = Vec::new();
            for path in &scan_paths {
                if let Ok(content) = std::fs::read_to_string(workspace_root.join(path)) {
                    sources.push((path.clone(), content));
                }
            }
            let source_refs: Vec<(&str, &str)> = sources
                .iter()
                .map(|(p, c)| (p.as_str(), c.as_str()))
                .collect();
            findings.extend(super::patterns::check_forbidden_patterns(
                &all_constraints,
                &source_refs,
                registry,
            ));
        }
    }

    let (mut active, waived) = waivers::partition(findings, &constraint_waivers);
    active.extend(check_ratchet_violations(
        &active_ratchets_from_conn(conn),
        &all_constraints,
    ));

    let mut all_active = parse_error_findings;
    all_active.append(&mut active);

    Ok(CheckOutcome {
        active: all_active,
        waived,
        parse_errors,
        ..Default::default()
    })
}

fn check_ratchet_violations(
    ratchets: &[ConstraintRatchetRow],
    constraints: &[Constraint],
) -> Vec<ConstraintFinding> {
    let constraint_map: HashMap<&str, &Constraint> =
        constraints.iter().map(|c| (&*c.id, c)).collect();

    let mut findings = Vec::new();
    for r in ratchets {
        match constraint_map.get(&*r.constraint_id) {
            None => {
                findings.push(ConstraintFinding {
                    constraint_id: Arc::clone(&r.constraint_id),
                    constraint_name: r.name.as_ref().map(Arc::clone),
                    constraint_kind: "ratchet_violation".into(),
                    severity: Severity::Blocking,
                    provenance: None,
                    from_path: String::new(),
                    to_path: String::new(),
                    component_context: None,
                    detail: format!(
                        "ratcheted constraint removed or modified: {} — was: {}. \
                         A human must run `sutra ratchet release {}` to retire it.",
                        r.name.as_deref().unwrap_or(&r.constraint_id),
                        r.rendered_description,
                        r.constraint_id,
                    ),
                    delta: FindingDelta::Unknown,
                    line: None,
                    snippet: None,
                    enclosing_symbol: None,
                });
            }
            Some(c) => {
                let floor =
                    Severity::from_str_lossy(&r.severity_floor).unwrap_or(Severity::Informational);
                if c.severity.ordinal() < floor.ordinal() {
                    findings.push(ConstraintFinding {
                        constraint_id: Arc::clone(&r.constraint_id),
                        constraint_name: r.name.as_ref().map(Arc::clone),
                        constraint_kind: "ratchet_violation".into(),
                        severity: Severity::Blocking,
                        provenance: c.provenance.as_ref().map(Arc::clone),
                        from_path: String::new(),
                        to_path: String::new(),
                        component_context: None,
                        detail: format!(
                            "ratcheted constraint severity downgraded: {} is now {} \
                             but floor is {}. A human must run `sutra ratchet release {}` \
                             to retire it.",
                            r.name.as_deref().unwrap_or(&r.constraint_id),
                            c.severity.as_str(),
                            r.severity_floor,
                            r.constraint_id,
                        ),
                        delta: FindingDelta::Unknown,
                        line: None,
                        snippet: None,
                        enclosing_symbol: None,
                    });
                }
            }
        }
    }
    findings
}

/// Check a manifest's findings against waivers and return a partitioned outcome.
fn partition_manifest_findings(
    conn: &rusqlite::Connection,
    manifest_rel_path: &str,
    findings: Vec<ConstraintFinding>,
) -> Result<(
    Vec<ConstraintFinding>,
    Vec<waivers::Waived<ConstraintFinding>>,
)> {
    use crate::db::ConstraintWaiverRow;
    use rusqlite::params;

    if findings.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
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
    Ok(waivers::partition(findings, &constraint_waivers))
}

/// Check a single (possibly proposed) Cargo manifest against external-crate
/// constraints, with waiver partitioning. Used by the guard on Cargo.toml edits.
///
/// When checking the root Cargo.toml, also re-checks member manifests if the
/// proposed edit changes workspace dependency renames — a rename change can
/// cause `workspace = true` aliases in members to resolve to constrained packages.
pub fn check_manifest_raw(
    conn: &rusqlite::Connection,
    workspace_root: &Path,
    manifest_rel_path: &str,
    content: &str,
) -> Result<CheckOutcome> {
    let mut loaded_rules = rules::load_rules(workspace_root)?;
    let (all_constraints, parse_errors) = loaded_rules.all_constraints();
    if !external::has_external_constraints(&all_constraints) {
        return Ok(CheckOutcome {
            parse_errors,
            ..Default::default()
        });
    }

    let layout = crate::rust_imports::parse_workspace_layout(workspace_root);
    let crate_names = layout.all_crate_names();
    let crate_name_refs: Vec<&str> = crate_names.to_vec();

    let is_root = manifest_rel_path == "Cargo.toml";
    let on_disk_root = std::fs::read_to_string(workspace_root.join("Cargo.toml")).ok();
    // The proposed content is the truth for the manifest under edit.
    let root_content: Option<&str> = if is_root {
        Some(content)
    } else {
        on_disk_root.as_deref()
    };
    let ws_renames = root_content
        .map(external::workspace_dep_renames)
        .unwrap_or_default();
    let renames = if ws_renames.is_empty() {
        None
    } else {
        Some(&ws_renames)
    };

    // Confinement ownership needs the workspace's package layout (sutra/291),
    // discovered the same way the index does it so the guard predicts what the
    // next audit will report.
    let manifest_paths = external::package_dirs_including(workspace_root, manifest_rel_path);
    let package_dirs = external::package_dirs_of(manifest_paths.iter().map(String::as_str));

    let mut findings = external::check_manifest(
        &all_constraints,
        manifest_rel_path,
        content,
        renames,
        &package_dirs,
    );
    if let Err(msg) =
        external::validate_no_external_targeting_members(&all_constraints, &crate_name_refs)
    {
        findings.push(external::config_error_finding(&msg));
    }

    let (mut active, waived) = partition_manifest_findings(conn, manifest_rel_path, findings)?;

    // When the root manifest's rename map changed, re-check declared workspace
    // members with the proposed renames — a new/changed alias may resolve to a
    // constrained package in members that use `workspace = true`.
    // Only surfaces *newly introduced* findings (not pre-existing violations).
    if is_root && !ws_renames.is_empty() {
        let old_renames = on_disk_root
            .as_deref()
            .map(external::workspace_dep_renames)
            .unwrap_or_default();
        if ws_renames != old_renames {
            let old_rename_ref = if old_renames.is_empty() {
                None
            } else {
                Some(&old_renames)
            };
            for (member_rel, member_content) in
                external::workspace_member_manifests(workspace_root, content)
            {
                let old_findings = external::check_manifest(
                    &all_constraints,
                    &member_rel,
                    &member_content,
                    old_rename_ref,
                    &package_dirs,
                );
                let old_keys: std::collections::HashSet<(&str, &str)> = old_findings
                    .iter()
                    .map(|f| (&*f.constraint_id, f.to_path.as_str()))
                    .collect();
                let new_findings: Vec<_> = external::check_manifest(
                    &all_constraints,
                    &member_rel,
                    &member_content,
                    renames,
                    &package_dirs,
                )
                .into_iter()
                .filter(|f| !old_keys.contains(&(&*f.constraint_id, f.to_path.as_str())))
                .collect();
                let (member_active, _) =
                    partition_manifest_findings(conn, &member_rel, new_findings)?;
                active.extend(member_active);
            }
        }
    }

    Ok(CheckOutcome {
        active,
        waived,
        parse_errors,
        ..Default::default()
    })
}

/// Check a single pubspec.yaml against external-crate constraints,
/// with waiver partitioning. Used by the guard on pubspec.yaml edits.
pub fn check_pubspec_raw(
    conn: &rusqlite::Connection,
    workspace_root: &Path,
    pubspec_rel_path: &str,
    content: &str,
) -> Result<CheckOutcome> {
    use rusqlite::params;

    let mut loaded_rules = rules::load_rules(workspace_root)?;
    let (all_constraints, parse_errors) = loaded_rules.all_constraints();
    if !external::has_external_constraints(&all_constraints) {
        return Ok(CheckOutcome {
            parse_errors,
            ..Default::default()
        });
    }

    // Same package layout the index side uses, so guard and audit agree on
    // confinement ownership (sutra/291).
    let pubspec_paths = external::package_dirs_including(workspace_root, pubspec_rel_path);
    let package_dirs = external::package_dirs_of(pubspec_paths.iter().map(String::as_str));

    let findings =
        external::check_pubspec(&all_constraints, pubspec_rel_path, content, &package_dirs);
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
        .query_map(params![pubspec_rel_path], |row| {
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

fn build_component_rows_raw(
    conn: &rusqlite::Connection,
) -> Result<Vec<(String, String, Vec<String>)>> {
    let mut out = Vec::new();
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
        let paths = json
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();
        out.push((id, name, paths));
    }
    Ok(out)
}

fn make_finding(
    c: &Constraint,
    from_path: &str,
    to_path: &str,
    file_to_component: &HashMap<&str, &str>,
    delta: FindingDelta,
) -> ConstraintFinding {
    ConstraintFinding {
        constraint_id: Arc::clone(&c.id),
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
        line: None,
        snippet: None,
        enclosing_symbol: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Workspace with a root package declaring `rusqlite` and a nested package
    /// that is NOT a declared `[workspace].members` entry — the shape where a
    /// members-derived package layout and a disk-derived one disagree.
    fn nested_non_member_workspace(allowed_in: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".sutra")).unwrap();
        std::fs::write(
            root.join(".sutra/rules.toml"),
            format!(
                "[[constraint]]\nkind = \"confined_external\"\n\
                 crates = [\"rusqlite\"]\nallowed_in = [{allowed_in}]\n\
                 severity = \"blocking\"\nname = \"sqlite-confined\"\n"
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"top\"\n\n[dependencies]\nrusqlite = \"0.32\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("vendor/client")).unwrap();
        std::fs::write(
            root.join("vendor/client/Cargo.toml"),
            "[package]\nname = \"client\"\n",
        )
        .unwrap();
        dir
    }

    fn waiver_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, \
             constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, \
             rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', \
             created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');",
        )
        .unwrap();
        conn
    }

    /// Findings the index would report for the root manifest, via the public
    /// workspace entry point.
    fn index_root_findings(workspace_root: &Path) -> usize {
        let mut loaded = rules::load_rules(workspace_root).unwrap();
        let (cs, _) = loaded.all_constraints();
        external::check_workspace_externals(&cs, workspace_root, &[], None, &[])
            .into_iter()
            .filter(|f| f.from_path == "Cargo.toml")
            .count()
    }

    fn guard_root_findings(conn: &rusqlite::Connection, workspace_root: &Path) -> usize {
        let content = std::fs::read_to_string(workspace_root.join("Cargo.toml")).unwrap();
        check_manifest_raw(conn, workspace_root, "Cargo.toml", &content)
            .unwrap()
            .active
            .into_iter()
            .filter(|f| f.from_path == "Cargo.toml")
            .count()
    }

    #[test]
    fn guard_and_index_agree_when_a_nested_non_member_owns_the_confinement() {
        // A nested package outside `[workspace].members` is invisible to a
        // members-derived layout: the guard would hand ownership to the root and
        // exempt the dependency the index still reports (sutra/291 review).
        let dir = nested_non_member_workspace("\"vendor/client/src/**\"");
        let conn = waiver_conn();
        let index = index_root_findings(dir.path());
        let guard = guard_root_findings(&conn, dir.path());
        assert_eq!(
            (guard, index),
            (1, 1),
            "the root does not own vendor/client's confinement, and both sides must say so"
        );
    }

    #[test]
    fn guard_and_index_agree_when_the_declaring_package_owns_the_confinement() {
        let dir = nested_non_member_workspace("\"src/db.rs\"");
        let conn = waiver_conn();
        assert_eq!(
            (
                guard_root_findings(&conn, dir.path()),
                index_root_findings(dir.path())
            ),
            (0, 0),
            "the root owns src/db.rs, so its own manifest entry is exempt on both sides"
        );
    }
}
