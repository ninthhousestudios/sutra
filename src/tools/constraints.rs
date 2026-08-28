use std::collections::HashMap;
use std::path::Path;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

use crate::constraints::DdEngine;
use crate::constraints::accepted::{self, AckEntry, WaiverEntry};
use crate::constraints::check::{self, EvalScope, FactsSource};
use crate::db::Db;
use crate::error::{Result, SutraError};
use crate::rules::{self, ConstraintKind};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConstraintsArgs {
    #[serde(default)]
    pub workspace: String,
    /// Action: "list", "violations", "waive", "unwaive", "baseline", "ack", "unack",
    /// "ack-cycle", "unack-cycle". Waivers and acks are persisted to
    /// `.sutra/accepted.toml` (version-controlled, keyed by constraint NAME); the DB
    /// is a rebuilt-on-read projection. `unwaive` and `unack` remove by content KEY,
    /// not a row id — the projection re-mints ids on every sync, so ids are not
    /// stable handles. `ack-cycle` accepts one import cycle by its file-set
    /// (`members`) so it drops off the report; a reshaped cycle re-surfaces.
    pub action: String,
    /// Constraint ID — 8-char blake3 hash (for waive/baseline/ack; resolved to the
    /// constraint's name, which is what the accepted file keys on)
    #[serde(default)]
    pub constraint_id: Option<String>,
    /// Human-readable constraint name — the accepted-file key. Alternative to
    /// constraint_id for waive/unwaive/baseline/ack/unack.
    #[serde(default)]
    pub constraint_name: Option<String>,
    /// File path the waiver/ack applies to (for waive/unwaive/ack/unack)
    #[serde(default)]
    pub file_path: Option<String>,
    /// Symbol qualified name scoping a waiver (for waive; part of the unwaive key)
    #[serde(default)]
    pub symbol_qualified_name: Option<String>,
    /// Rationale for the waiver/ack (required for waive and ack; optional for baseline)
    #[serde(default)]
    pub rationale: Option<String>,
    /// Who is granting the waiver (for waive)
    #[serde(default)]
    pub waived_by: Option<String>,
    /// Who is acknowledging the instance(s) (for baseline/ack)
    #[serde(default)]
    pub acked_by: Option<String>,
    /// Line of the specific match instance to ack (for ack; or use `snippet`)
    #[serde(default)]
    pub line: Option<u32>,
    /// Snippet (matched node's first line) selecting the instance to ack, and part
    /// of the unack content key (for ack/unack)
    #[serde(default)]
    pub snippet: Option<String>,
    /// Glob restricting which files a baseline snapshots (optional, for baseline)
    #[serde(default)]
    pub scope: Option<String>,
    /// The member file paths of an import cycle to accept or revoke (for
    /// ack-cycle / unack-cycle). Order-insensitive — a cycle's identity is its
    /// file *set*; pass the exact members shown in the `violations` report.
    #[serde(default)]
    pub members: Option<Vec<String>>,
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
        "waive" => handle_waive(db, workspace_root, args),
        "unwaive" => handle_unwaive(db, workspace_root, args),
        "baseline" => handle_baseline(db, workspace_root, dd_engine, args),
        "ack" => handle_ack(db, workspace_root, args),
        "unack" => handle_unack(db, workspace_root, args),
        "ack-cycle" => handle_ack_cycle(db, workspace_root, dd_engine, args),
        "unack-cycle" => handle_unack_cycle(db, workspace_root, args),
        other => Err(SutraError::Internal(format!(
            "unknown action: {other}. expected: list, violations, waive, unwaive, \
             baseline, ack, unack, ack-cycle, unack-cycle"
        ))),
    }
}

fn handle_list(db: &Db, workspace_root: &Path) -> Result<serde_json::Value> {
    use crate::constraints::constraint_coverage;

    let mut rules = rules::load_rules(workspace_root)?;
    let (all_constraints, constraint_parse_errors) = rules.all_constraints();

    // `list` reads the waiver cache directly (below), so gate freshness here — the
    // report chokepoint in `check::evaluate` does not run on this path.
    let accepted_warnings: Vec<String> =
        accepted::refresh_cache(db, workspace_root, &all_constraints)?
            .iter()
            .map(accepted::AcceptedWarning::message)
            .collect();

    let waivers = db.get_constraint_waivers(None)?;

    let mut waiver_counts: HashMap<&str, usize> = HashMap::new();
    for w in &waivers {
        *waiver_counts.entry(&w.constraint_id).or_default() += 1;
    }

    let ratchets = db.get_active_constraint_ratchets().unwrap_or_default();
    let ratchet_map: HashMap<&str, &crate::db::ConstraintRatchetRow> =
        ratchets.iter().map(|r| (&*r.constraint_id, r)).collect();

    let all_files = db.all_files()?;
    let paths: Vec<&str> = all_files.iter().map(|f| &*f.path).collect();

    // Unindexed stubs are absent from the files table, so a stub-only pattern
    // rule would report zero coverage and be flagged inert — contradicting the
    // violations action, which does scan them.
    let has_patterns = all_constraints
        .iter()
        .any(|c| matches!(c.kind, ConstraintKind::ForbiddenPattern { .. }));
    let stub_paths: Vec<String> = if has_patterns {
        crate::constraints::patterns::scan_pattern_only_files(
            workspace_root,
            &crate::parser::adapter::default_registry(),
        )
    } else {
        Vec::new()
    };
    let stub_path_refs: Vec<&str> = stub_paths.iter().map(|p| p.as_str()).collect();

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

            let coverage = constraint_coverage(
                c,
                &paths,
                &stub_path_refs,
                &component_names,
                &component_ids,
            );
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
            if let Some(r) = ratchet_map.get(&*c.id) {
                entry["ratcheted"] = json!(true);
                entry["severity_floor"] = json!(&r.severity_floor);
            }
            entry
        })
        .collect();

    let mut result = json!({ "constraints": constraints_out });
    if !ratchets.is_empty() {
        result["active_ratchet_count"] = json!(ratchets.len());
    }
    if !accepted_warnings.is_empty() {
        result["accepted_warnings"] = json!(accepted_warnings);
    }
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
    // Surface instance acks so the state that removed matches from `violations`
    // is visible, not silent (sutra/305). Errors propagate (sutra/306): a failed
    // ack lookup must not masquerade as "nothing acked".
    let acked = acked_instances_json(db, None)?;
    if !acked.is_empty() {
        result["acknowledged"] = json!(acked);
    }
    // Config warnings from resolving accepted.toml (unknown/ambiguous constraint
    // refs) — surfaced, not dropped (sutra/308 hazard 4).
    if !outcome.accepted_warnings.is_empty() {
        result["accepted_warnings"] = json!(outcome.accepted_warnings);
    }
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

/// The accepted-file key for a constraint: its human-stable NAME, or its id when
/// it has no name. Used by removal paths (`unwaive`, `unack`) that must match
/// legacy migrated entries which may have been keyed by id.
fn accepted_key(constraint_id: &str, constraint_name: Option<&str>) -> String {
    constraint_name
        .map(str::to_string)
        .unwrap_or_else(|| constraint_id.to_string())
}

/// Like `accepted_key`, but rejects nameless constraints with an actionable error.
/// Used by write paths (`waive`, `baseline`, `ack`) where an id-keyed entry would
/// silently resolve as Unknown on the next load and never take effect — the waiver
/// or ack appears to succeed but is inert (sutra/310).
fn require_named_constraint(constraint_id: &str, constraint_name: Option<&str>) -> Result<String> {
    constraint_name.map(str::to_string).ok_or_else(|| {
        SutraError::Internal(format!(
            "constraint {constraint_id} has no name — add `name = \"...\"` to its \
             rule in .sutra/rules.toml before waiving or acking (portable waivers/acks \
             key by name, not the blake3 id)"
        ))
    })
}

/// Write a guard-honored waiver to `.sutra/accepted.toml` and re-project the cache
/// so it governs the very next report. The DB write is gone — the file is the
/// source of truth (sutra/303). `migrate_db_to_file` runs first so any legacy
/// DB-only waivers are seeded into the file before this one is appended; skip it
/// and an absent file would be created carrying only the new entry, and the
/// re-projection would wipe the rest (sutra/308 hazard 1).
fn handle_waive(
    db: &Db,
    workspace_root: &Path,
    args: &ConstraintsArgs,
) -> Result<serde_json::Value> {
    let file_path = args
        .file_path
        .as_deref()
        .ok_or_else(|| SutraError::Internal("waive requires file_path".into()))?;
    let rationale = args
        .rationale
        .as_deref()
        .ok_or_else(|| SutraError::Internal("waive requires rationale".into()))?;
    let waived_by = args
        .waived_by
        .as_deref()
        .ok_or_else(|| SutraError::Internal("waive requires waived_by".into()))?;

    let mut rules_loaded = rules::load_rules(workspace_root)?;
    let (all_constraints, _errors) = rules_loaded.all_constraints();
    let (constraint_id, constraint_name) = resolve_constraint(
        &all_constraints,
        args.constraint_id.as_deref(),
        args.constraint_name.as_deref(),
    )?;
    let key = require_named_constraint(&constraint_id, constraint_name.as_deref())?;
    let result = json!({
        "waived": key,
        "constraint_id": constraint_id,
        "file_path": file_path,
    });

    accepted::migrate_db_to_file(db, workspace_root)?;
    accepted::upsert_waiver(
        workspace_root,
        WaiverEntry {
            constraint: key.to_string(),
            file: file_path.to_string(),
            symbol: args.symbol_qualified_name.as_deref().map(str::to_string),
            rationale: rationale.to_string(),
            by: waived_by.to_string(),
        },
    )?;
    accepted::ensure_cache_fresh(db, workspace_root, &all_constraints)?;

    Ok(result)
}

/// Remove a waiver by its `(constraint, file, symbol)` content key and re-project.
/// Key-based, not id-based: the projection re-mints row ids on every sync, so an
/// id is not a stable handle (sutra/308 unit F).
fn handle_unwaive(
    db: &Db,
    workspace_root: &Path,
    args: &ConstraintsArgs,
) -> Result<serde_json::Value> {
    let file_path = args
        .file_path
        .as_deref()
        .ok_or_else(|| SutraError::Internal("unwaive requires file_path".into()))?;

    let mut rules_loaded = rules::load_rules(workspace_root)?;
    let (all_constraints, _errors) = rules_loaded.all_constraints();
    let (constraint_id, constraint_name) = resolve_constraint(
        &all_constraints,
        args.constraint_id.as_deref(),
        args.constraint_name.as_deref(),
    )?;
    let key = accepted_key(&constraint_id, constraint_name.as_deref());

    accepted::migrate_db_to_file(db, workspace_root)?;
    let removed = accepted::remove_waiver(
        workspace_root,
        &key,
        file_path,
        args.symbol_qualified_name.as_deref(),
    )?;
    if !removed {
        return Err(SutraError::Internal(format!(
            "no waiver for constraint '{key}' on {file_path} (symbol {:?})",
            args.symbol_qualified_name
        )));
    }
    accepted::ensure_cache_fresh(db, workspace_root, &all_constraints)?;

    Ok(json!({ "revoked": key, "file_path": file_path }))
}

pub(crate) fn ack_to_json(a: &crate::db::ConstraintInstanceAckRow) -> serde_json::Value {
    json!({
        "ack_id": a.id,
        "constraint_id": a.constraint_id,
        "constraint_name": a.constraint_name,
        "file_path": a.file_path,
        "enclosing_symbol": a.enclosing_symbol,
        "snippet": a.snippet,
        "accepted_count": a.accepted_count,
        "rationale": a.rationale,
        "acked_by": a.acked_by,
    })
}

/// Report-only instance acks as JSON, for surfacing on any report surface
/// (`violations` / review / orient) so the state that removed matches from the
/// report stays visible, not silent (sutra/305, sutra/306). `scope` restricts to
/// acks on the given file paths (`None` = all); errors propagate rather than
/// being swallowed, so a failed lookup is never mistaken for "no acks".
pub(crate) fn acked_instances_json(
    db: &Db,
    scope: Option<&std::collections::HashSet<&str>>,
) -> Result<Vec<serde_json::Value>> {
    Ok(db
        .get_all_constraint_instance_acks()?
        .iter()
        .filter(|a| {
            scope
                .map(|s| s.contains(a.file_path.as_str()))
                .unwrap_or(true)
        })
        .map(ack_to_json)
        .collect())
}

/// Resolve a constraint by id (preferred) or name from already-loaded rules,
/// returning its `(id, name)`. Baseline/ack accept either handle.
fn resolve_constraint(
    all_constraints: &[rules::Constraint],
    constraint_id: Option<&str>,
    constraint_name: Option<&str>,
) -> Result<(String, Option<String>)> {
    let found = all_constraints.iter().find(|c| match constraint_id {
        Some(id) => c.id.as_ref() == id,
        None => match constraint_name {
            Some(name) => c.name.as_deref() == Some(name),
            None => false,
        },
    });
    match found {
        Some(c) => Ok((c.id.to_string(), c.name.as_deref().map(str::to_string))),
        None => Err(SutraError::Internal(format!(
            "no constraint matches id={constraint_id:?} name={constraint_name:?}"
        ))),
    }
}

/// Bulk-acknowledge every currently-matching instance of one forbidden_pattern
/// constraint (optionally within `scope`) as examined-and-accepted — the answer
/// to "clear these 95 owned-required clones off the report without blinding the
/// rule." One ack row per content key, `accepted_count` = how many matches share
/// it, so a future byte-identical clone (count+1) still surfaces (sutra/305).
fn handle_baseline(
    db: &Db,
    workspace_root: &Path,
    _dd_engine: Option<&DdEngine>,
    args: &ConstraintsArgs,
) -> Result<serde_json::Value> {
    let acked_by = args
        .acked_by
        .as_deref()
        .ok_or_else(|| SutraError::Internal("baseline requires acked_by".into()))?;

    let registry = crate::parser::adapter::default_registry();
    let mut rules_loaded = rules::load_rules(workspace_root)?;
    let (all_constraints, _errors) = rules_loaded.all_constraints();
    let (constraint_id, constraint_name) = resolve_constraint(
        &all_constraints,
        args.constraint_id.as_deref(),
        args.constraint_name.as_deref(),
    )?;
    let key = require_named_constraint(&constraint_id, constraint_name.as_deref())?;

    // Candidate files: every indexed file plus unindexed pattern-only stubs,
    // narrowed by the optional action scope. check_forbidden_patterns applies the
    // constraint's own scope, extension, and test exclusion on top.
    let all_files = db.all_files()?;
    let scope = args.scope.as_deref();
    let mut sources: Vec<(String, String)> = Vec::new();
    for f in &all_files {
        if let Some(sc) = scope
            && !rules::scope_matches_path(sc, &f.path)
        {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(workspace_root.join(&*f.path)) {
            sources.push((f.path.to_string(), content));
        }
    }
    for path in crate::constraints::patterns::scan_pattern_only_files(workspace_root, &registry) {
        if let Some(sc) = scope
            && !rules::scope_matches_path(sc, &path)
        {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(workspace_root.join(&path)) {
            sources.push((path, content));
        }
    }
    let source_refs: Vec<(&str, &str)> = sources
        .iter()
        .map(|(p, c)| (p.as_str(), c.as_str()))
        .collect();

    let findings = crate::constraints::patterns::check_forbidden_patterns(
        &all_constraints,
        &source_refs,
        &registry,
    );
    let mut target: Vec<_> = findings
        .into_iter()
        .filter(|f| f.constraint_id.as_ref() == constraint_id)
        .collect();

    // Group by (file, enclosing, snippet) without cloning keys: sort, then count
    // contiguous runs. One upsert per key, accepted_count = run length.
    target.sort_by(|a, b| {
        a.from_path
            .cmp(&b.from_path)
            .then(a.enclosing_symbol.cmp(&b.enclosing_symbol))
            .then(a.snippet.cmp(&b.snippet))
    });
    // Seed any legacy DB-only acks/waivers into the file before appending these,
    // then batch-upsert all entries in one file write cycle (sutra/310) with a
    // single re-projection at the end (sutra/308 unit F, hazard 1).
    accepted::migrate_db_to_file(db, workspace_root)?;
    let mut keys_acked = 0usize;
    let mut instances = 0i64;
    let mut batch = Vec::new();
    let mut i = 0;
    while i < target.len() {
        let mut j = i + 1;
        while j < target.len()
            && target[j].from_path == target[i].from_path
            && target[j].enclosing_symbol == target[i].enclosing_symbol
            && target[j].snippet == target[i].snippet
        {
            j += 1;
        }
        let count = (j - i) as u32;
        let f = &target[i];
        batch.push(AckEntry {
            constraint: key.to_string(),
            file: f.from_path.to_string(),
            symbol: f.enclosing_symbol.as_deref().map(str::to_string),
            snippet: f.snippet.as_deref().map(str::to_string),
            count,
            rationale: args.rationale.as_deref().map(str::to_string),
            by: acked_by.to_string(),
        });
        keys_acked += 1;
        instances += i64::from(count);
        i = j;
    }
    accepted::upsert_acks_batch(workspace_root, batch)?;
    accepted::ensure_cache_fresh(db, workspace_root, &all_constraints)?;

    Ok(json!({
        "constraint_id": constraint_id,
        "constraint_name": constraint_name,
        "keys_acked": keys_acked,
        "instances_acked": instances,
    }))
}

/// Acknowledge a single examined instance (selected by `line` or `snippet`) with
/// a required rationale. Increments the content key's accepted_count by one,
/// capped at how many matches share that key on disk so repeated or stale acks
/// can never suppress more than actually exists — a new clone always resurfaces
/// (sutra/305).
fn handle_ack(db: &Db, workspace_root: &Path, args: &ConstraintsArgs) -> Result<serde_json::Value> {
    let file_path = args
        .file_path
        .as_deref()
        .ok_or_else(|| SutraError::Internal("ack requires file_path".into()))?;
    let rationale = args
        .rationale
        .as_deref()
        .ok_or_else(|| SutraError::Internal("ack requires rationale".into()))?;
    let acked_by = args
        .acked_by
        .as_deref()
        .ok_or_else(|| SutraError::Internal("ack requires acked_by".into()))?;
    if args.line.is_none() && args.snippet.is_none() {
        return Err(SutraError::Internal(
            "ack requires line or snippet to select the instance".into(),
        ));
    }

    let registry = crate::parser::adapter::default_registry();
    let mut rules_loaded = rules::load_rules(workspace_root)?;
    let (all_constraints, _errors) = rules_loaded.all_constraints();
    let (constraint_id, constraint_name) = resolve_constraint(
        &all_constraints,
        args.constraint_id.as_deref(),
        args.constraint_name.as_deref(),
    )?;

    let content = std::fs::read_to_string(workspace_root.join(file_path))
        .map_err(|e| SutraError::Internal(format!("cannot read {file_path}: {e}")))?;
    let findings = crate::constraints::patterns::check_forbidden_patterns(
        &all_constraints,
        &[(file_path, &content)],
        &registry,
    );
    let target: Vec<_> = findings
        .iter()
        .filter(|f| f.constraint_id.as_ref() == constraint_id)
        .collect();

    let selected = match (args.line, args.snippet.as_deref()) {
        (Some(line), _) => target.iter().find(|f| f.line == Some(line)),
        (None, Some(snip)) => target.iter().find(|f| f.snippet.as_deref() == Some(snip)),
        (None, None) => None,
    }
    .copied()
    .ok_or_else(|| {
        SutraError::Internal(format!(
            "no {constraint_id} match at the requested instance in {file_path}"
        ))
    })?;

    // How many matches share this content key on disk — the cap.
    let matched = target
        .iter()
        .filter(|f| {
            f.enclosing_symbol == selected.enclosing_symbol && f.snippet == selected.snippet
        })
        .count() as i64;
    let key = require_named_constraint(&constraint_id, constraint_name.as_deref())?;

    // Read the current count off a cache coherent with the file (migrate seeds
    // legacy rows, ensure reprojects) so the +1 caps against the real prior ack
    // (sutra/308 unit F, hazard 1).
    accepted::refresh_cache(db, workspace_root, &all_constraints)?;
    // Existing accepted_count for this key, if any.
    let existing = db
        .get_constraint_instance_acks_for_file(file_path)?
        .into_iter()
        .find(|a| {
            a.constraint_id.as_ref() == constraint_id
                && a.enclosing_symbol.as_deref() == selected.enclosing_symbol.as_deref()
                && a.snippet.as_deref() == selected.snippet.as_deref()
        })
        .map(|a| a.accepted_count)
        .unwrap_or(0);
    let new_count = (existing + 1).min(matched);
    // Build the ack before the key is moved into the entry below.
    let result = json!({
        "acked": key,
        "constraint_id": constraint_id,
        "file_path": file_path,
        "enclosing_symbol": selected.enclosing_symbol,
        "snippet": selected.snippet,
        "accepted_count": new_count,
        "matched": matched,
    });

    accepted::upsert_ack(
        workspace_root,
        AckEntry {
            constraint: key,
            file: file_path.to_string(),
            symbol: selected.enclosing_symbol.as_deref().map(str::to_string),
            snippet: selected.snippet.as_deref().map(str::to_string),
            count: new_count as u32,
            rationale: Some(rationale.to_string()),
            by: acked_by.to_string(),
        },
    )?;
    accepted::ensure_cache_fresh(db, workspace_root, &all_constraints)?;

    Ok(result)
}

/// Remove an ack by its `(constraint, file, symbol, snippet)` content key and
/// re-project. Key-based, not id-based, for the same reason as `unwaive`: the
/// projection re-mints row ids on every sync (sutra/308 unit F).
fn handle_unack(
    db: &Db,
    workspace_root: &Path,
    args: &ConstraintsArgs,
) -> Result<serde_json::Value> {
    let file_path = args
        .file_path
        .as_deref()
        .ok_or_else(|| SutraError::Internal("unack requires file_path".into()))?;

    let mut rules_loaded = rules::load_rules(workspace_root)?;
    let (all_constraints, _errors) = rules_loaded.all_constraints();
    let (constraint_id, constraint_name) = resolve_constraint(
        &all_constraints,
        args.constraint_id.as_deref(),
        args.constraint_name.as_deref(),
    )?;
    let key = accepted_key(&constraint_id, constraint_name.as_deref());

    accepted::migrate_db_to_file(db, workspace_root)?;
    let removed = accepted::remove_ack(
        workspace_root,
        &key,
        file_path,
        args.symbol_qualified_name.as_deref(),
        args.snippet.as_deref(),
    )?;
    if !removed {
        return Err(SutraError::Internal(format!(
            "no ack for constraint '{key}' on {file_path} (symbol {:?}, snippet {:?})",
            args.symbol_qualified_name, args.snippet
        )));
    }
    accepted::ensure_cache_fresh(db, workspace_root, &all_constraints)?;

    Ok(json!({ "revoked_ack": key, "file_path": file_path }))
}

/// Accept a single import cycle by its file-set so it drops off the report, while
/// a reshaped cycle (a member added or removed) re-surfaces — the answer to "this
/// known-good cycle is un-owned so it has no name to waive, no longer gates
/// (sutra/359), yet still clutters the report" (sutra/360). Keyed by the cycle's
/// fingerprint, not a leaky single path (sutra/359 problem 2). The set is
/// re-verified against the live report so a typo'd or stale file-set cannot seed a
/// phantom ack; the owned/un-owned constraint identity is read off the matching
/// finding, so the operator need not name the constraint.
fn handle_ack_cycle(
    db: &Db,
    workspace_root: &Path,
    dd_engine: Option<&DdEngine>,
    args: &ConstraintsArgs,
) -> Result<serde_json::Value> {
    let members = args
        .members
        .as_deref()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            SutraError::Internal("ack-cycle requires members (the cycle's file set)".into())
        })?;
    let rationale = args
        .rationale
        .as_deref()
        .ok_or_else(|| SutraError::Internal("ack-cycle requires rationale".into()))?;
    let acked_by = args
        .acked_by
        .as_deref()
        .ok_or_else(|| SutraError::Internal("ack-cycle requires acked_by".into()))?;

    let refs: Vec<&str> = members.iter().map(String::as_str).collect();
    let fingerprint = check::cycle_fingerprint(&refs);

    // Re-verify: the file-set must name a cycle the live report actually shows, so
    // a wrong or stale set cannot strand an inert ack. The finding also carries the
    // cycle's constraint identity (an owned rule id, or the reserved builtin).
    let registry = crate::parser::adapter::default_registry();
    let outcome = check::evaluate(
        &FactsSource::DdBacked { db, dd_engine },
        workspace_root,
        EvalScope::Workspace,
        &registry,
    )?;
    let finding = outcome
        .active
        .iter()
        .find(|f| f.constraint_kind == "no_cycles" && f.snippet.as_deref() == Some(&fingerprint))
        .ok_or_else(|| {
            SutraError::Internal(format!(
                "no active import cycle matches [{fingerprint}] — pass the exact member set \
                 from the violations report (an already-acked or waived cycle won't match)"
            ))
        })?;

    // An owned cycle keys by its rule name (a nameless owned rule is rejected, as
    // for any ack — sutra/310); an un-owned cycle keys by the reserved builtin name,
    // the one sanctioned id-keyed entry (resolved by the carve-out in resolve_accepted).
    let key = if finding.constraint_id.as_ref() == check::BUILTIN_CYCLES_ID {
        check::BUILTIN_CYCLES_ID.to_string()
    } else {
        require_named_constraint(
            finding.constraint_id.as_ref(),
            finding.constraint_name.as_deref(),
        )?
    };
    // The ack's storage bucket is the cycle's lexically first member — identical to
    // the finding's `from_path` (the matched fingerprint proves the sets are equal),
    // computed from `refs` so no owned data is cloned out of the borrowed finding.
    let file = refs.iter().copied().min().unwrap_or("").to_string();
    let result = json!({
        "acked_cycle": &key,
        "constraint_id": finding.constraint_id.as_ref(),
        "file": &file,
        "members": &fingerprint,
    });

    accepted::migrate_db_to_file(db, workspace_root)?;
    accepted::upsert_ack(
        workspace_root,
        AckEntry {
            constraint: key,
            file,
            symbol: None,
            snippet: Some(fingerprint),
            count: 1,
            rationale: Some(rationale.to_string()),
            by: acked_by.to_string(),
        },
    )?;
    let mut rules_loaded = rules::load_rules(workspace_root)?;
    let (all_constraints, _errors) = rules_loaded.all_constraints();
    accepted::ensure_cache_fresh(db, workspace_root, &all_constraints)?;

    Ok(result)
}

/// Revoke a cycle ack by its file-set. Mirrors `ack-cycle`: canonicalize the
/// members to the same fingerprint, read the stored entry's constraint key from the
/// file (an owned rule's name or the reserved builtin), and remove by content key —
/// so removal works regardless of ownership without the operator naming the
/// constraint. The file is the source of truth, read directly.
fn handle_unack_cycle(
    db: &Db,
    workspace_root: &Path,
    args: &ConstraintsArgs,
) -> Result<serde_json::Value> {
    let members = args
        .members
        .as_deref()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| {
            SutraError::Internal("unack-cycle requires members (the cycle's file set)".into())
        })?;
    let refs: Vec<&str> = members.iter().map(String::as_str).collect();
    let fingerprint = check::cycle_fingerprint(&refs);
    // The ack's storage bucket is the cycle's lexically first member — the same
    // `from_path` the finding carries and `ack-cycle` wrote under.
    let file = refs.iter().copied().min().unwrap_or("").to_string();

    // Seed any legacy DB-only rows first (hazard 1), then read the entry to recover
    // its constraint key verbatim so removal matches an owned or builtin cycle alike.
    accepted::migrate_db_to_file(db, workspace_root)?;
    let existing = accepted::load_accepted_file(workspace_root)?;
    let key: &str = existing
        .acks
        .iter()
        .find(|a| {
            a.file == file && a.symbol.is_none() && a.snippet.as_deref() == Some(&fingerprint)
        })
        .map(|a| a.constraint.as_str())
        .ok_or_else(|| {
            SutraError::Internal(format!("no cycle ack matches [{fingerprint}] on {file}"))
        })?;

    let removed = accepted::remove_ack(workspace_root, key, &file, None, Some(&fingerprint))?;
    if !removed {
        return Err(SutraError::Internal(format!(
            "no cycle ack matches [{fingerprint}] on {file}"
        )));
    }
    let mut rules_loaded = rules::load_rules(workspace_root)?;
    let (all_constraints, _errors) = rules_loaded.all_constraints();
    accepted::ensure_cache_fresh(db, workspace_root, &all_constraints)?;

    Ok(json!({ "revoked_cycle_ack": key, "file": file, "members": fingerprint }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `list` and `violations` must agree on what a rule covers. Stubs are
    /// unindexed, so a stub-only pattern rule has zero rows in the files table —
    /// counting only those would flag an actively-firing rule as inert and invite
    /// someone to delete it.
    #[test]
    fn stub_only_pattern_rule_is_not_reported_inert() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_unchecked("test", dir.path()).unwrap();

        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_pattern"
language = "python"
query = '(function_definition) @match'
name = "stub-only-rule"
scope = "**/*.pyi"
"#,
        )
        .unwrap();

        let pkg = dir.path().join("python");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("api.pyi"), "def f() -> int: ...\n").unwrap();

        let out = handle_list(&db, dir.path()).unwrap();
        let entry = &out["constraints"][0];
        assert_eq!(entry["matched_file_count"]["scope"], 1);
        assert!(
            entry.get("warning").is_none(),
            "stub-only rule wrongly flagged inert: {entry:#?}"
        );
    }

    /// The inert warning must still fire when there is genuinely nothing to match.
    #[test]
    fn pattern_rule_with_no_files_is_reported_inert() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_unchecked("test", dir.path()).unwrap();

        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_pattern"
language = "python"
query = '(function_definition) @match'
name = "stub-only-rule"
scope = "**/*.pyi"
"#,
        )
        .unwrap();

        let out = handle_list(&db, dir.path()).unwrap();
        let entry = &out["constraints"][0];
        assert_eq!(entry["matched_file_count"]["scope"], 0);
        assert!(entry.get("warning").is_some());
    }

    /// Stub paths are pattern-only: letting a dep-kind glob count one would mask
    /// a forbidden_dep rule that can never fire (stubs have no imports).
    #[test]
    fn stub_paths_do_not_count_toward_dep_constraint_coverage() {
        use crate::constraints::constraint_coverage;
        use crate::rules::parse_rules;

        let toml = r#"
[[constraint]]
kind = "forbidden_dep"
from = "python/**"
to = "src/**"
"#;
        let c = &parse_rules(toml).unwrap().all_constraints().0[0];
        let cov = constraint_coverage(c, &[], &["python/api.pyi"], &[], &[]);
        assert_eq!(cov.total_matched(), 0);
    }

    // --- Instance acks (sutra/305) ---

    const CLONE_RULE_TOML: &str = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(call_expression function: (field_expression field: (field_identifier) @m (#eq? @m "clone"))) @match'
name = "no-clone"
severity = "blocking"
scope = "src/"
"#;

    /// Seed a workspace with the clone rule and one source file carrying two
    /// byte-identical clones plus one distinct clone.
    fn ack_workspace() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_unchecked("test", dir.path()).unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("rules.toml"), CLONE_RULE_TOML).unwrap();

        let src = "fn a() {\n    let x = foo.clone();\n    let y = foo.clone();\n    \
                   let w = bar.clone();\n}\n";
        let src_path = dir.path().join("src/lib.rs");
        std::fs::create_dir_all(src_path.parent().unwrap()).unwrap();
        std::fs::write(&src_path, src).unwrap();
        db.upsert_file("src/lib.rs", "rust", "h1", 5, true).unwrap();
        (dir, db)
    }

    fn active_pattern_count(v: &serde_json::Value) -> usize {
        v["violations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["constraint_kind"] == "forbidden_pattern")
            .count()
    }

    #[test]
    fn baseline_clears_current_matches_and_surfaces_state() {
        let (dir, db) = ack_workspace();

        let before = handle_violations(&db, dir.path(), None).unwrap();
        assert_eq!(active_pattern_count(&before), 3, "3 clones before baseline");

        let args = ConstraintsArgs {
            workspace: "test".into(),
            action: "baseline".into(),
            constraint_name: Some("no-clone".into()),
            acked_by: Some("josh".into()),
            ..blank_args()
        };
        let out = handle_baseline(&db, dir.path(), None, &args).unwrap();
        // Two content keys (foo.clone() x2, bar.clone() x1), three instances.
        assert_eq!(out["keys_acked"], 2);
        assert_eq!(out["instances_acked"], 3);

        let after = handle_violations(&db, dir.path(), None).unwrap();
        assert_eq!(
            active_pattern_count(&after),
            0,
            "baseline clears the report"
        );
        assert_eq!(
            after["acknowledged"].as_array().unwrap().len(),
            2,
            "acked state is surfaced, not silent"
        );
    }

    #[test]
    fn ack_requires_rationale_and_is_count_capped() {
        let (dir, db) = ack_workspace();

        // Base ack args; each case overrides only `line`/`rationale`.
        let ack_args = |line: Option<u32>, rationale: Option<&str>| ConstraintsArgs {
            workspace: "test".into(),
            action: "ack".into(),
            constraint_name: Some("no-clone".into()),
            file_path: Some("src/lib.rs".into()),
            acked_by: Some("josh".into()),
            line,
            rationale: rationale.map(str::to_string),
            ..blank_args()
        };

        // rationale is mandatory for a single-instance ack.
        assert!(handle_ack(&db, dir.path(), &ack_args(Some(2), None)).is_err());

        // Ack the first foo.clone() (line 2). One of two identical instances.
        let out = handle_ack(&db, dir.path(), &ack_args(Some(2), Some("owned-required"))).unwrap();
        assert_eq!(out["accepted_count"], 1);
        assert_eq!(out["matched"], 2);
        let mid = handle_violations(&db, dir.path(), None).unwrap();
        assert_eq!(
            active_pattern_count(&mid),
            2,
            "1 of 2 foo clones acked -> 1 foo surplus + bar remain"
        );

        // Ack the second identical instance (line 3): count -> 2, capped at 2.
        let ack2 = ack_args(Some(3), Some("owned-required"));
        let out2 = handle_ack(&db, dir.path(), &ack2).unwrap();
        assert_eq!(
            out2["accepted_count"], 2,
            "second ack increments the key count"
        );

        // A third ack of an identical instance cannot over-suppress: capped at 2.
        let out3 = handle_ack(&db, dir.path(), &ack2).unwrap();
        assert_eq!(
            out3["accepted_count"], 2,
            "accepted_count is capped at the number of matches on disk"
        );

        let after = handle_violations(&db, dir.path(), None).unwrap();
        assert_eq!(
            active_pattern_count(&after),
            1,
            "both foo clones acked -> only the distinct bar clone remains"
        );

        // unack the foo key -> both foo clones resurface. Removal is key-based
        // (constraint + file + symbol + snippet); the projection re-mints row ids
        // on every sync, so an id is not a stable handle (sutra/308 unit F).
        let foo = after
            .get("acknowledged")
            .and_then(|a| a.as_array())
            .and_then(|a| a.iter().find(|r| r["snippet"] == "foo.clone()"))
            .expect("foo ack surfaced");
        let unack = ConstraintsArgs {
            workspace: "test".into(),
            action: "unack".into(),
            constraint_name: Some("no-clone".into()),
            file_path: Some("src/lib.rs".into()),
            symbol_qualified_name: foo["enclosing_symbol"].as_str().map(str::to_string),
            snippet: Some("foo.clone()".into()),
            ..blank_args()
        };
        handle_unack(&db, dir.path(), &unack).unwrap();
        let restored = handle_violations(&db, dir.path(), None).unwrap();
        assert_eq!(
            active_pattern_count(&restored),
            3,
            "unack restores all matches"
        );
    }

    // --- Cycle acks by file-set (sutra/360) ---

    /// Seed a workspace whose `.sutra/rules.toml` is `rule` and whose import graph
    /// holds a production 2-cycle `src/a.rs <-> src/b.rs` (kind "use", so neither
    /// test-edge nor module-tree narrowing removes it).
    fn cycle_workspace(rule: &str) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_unchecked("test", dir.path()).unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("rules.toml"), rule).unwrap();
        for (p, h) in [("src/a.rs", "h1"), ("src/b.rs", "h2")] {
            db.upsert_file(p, "rust", h, 10, true).unwrap();
        }
        let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
        let fb = db.file_by_path("src/b.rs").unwrap().unwrap();
        db.insert_import(fa.id, "src/b.rs", Some(fb.id), 1, "use", None)
            .unwrap();
        db.insert_import(fb.id, "src/a.rs", Some(fa.id), 1, "use", None)
            .unwrap();
        (dir, db)
    }

    fn active_cycle_count(v: &serde_json::Value) -> usize {
        v["violations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["constraint_kind"] == "no_cycles")
            .count()
    }

    fn cycle_finding(v: &serde_json::Value) -> Option<&serde_json::Value> {
        v["violations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["constraint_kind"] == "no_cycles")
    }

    /// An un-owned cycle (no `no_cycles` rule covers it) is stamped `builtin:cycles`
    /// and has no name to waive — yet it can be acked by its file-set, drops off the
    /// report, persists to accepted.toml (survives a fresh evaluation that
    /// re-projects from the file), and is fully reversible with `unack-cycle`.
    #[test]
    fn unowned_cycle_ackable_by_file_set_persists_and_unack_restores() {
        let (dir, db) = cycle_workspace("");

        let before = handle_violations(&db, dir.path(), None).unwrap();
        let cyc = cycle_finding(&before).expect("un-owned cycle reported");
        assert_eq!(cyc["constraint_id"], "builtin:cycles");
        assert_eq!(cyc["snippet"], "src/a.rs -> src/b.rs");

        // A bogus set must not seed a phantom ack — the re-verify guard rejects it.
        let bogus = ConstraintsArgs {
            action: "ack-cycle".into(),
            members: Some(vec!["src/a.rs".into(), "src/nope.rs".into()]),
            rationale: Some("x".into()),
            acked_by: Some("josh".into()),
            ..blank_args()
        };
        assert!(handle_ack_cycle(&db, dir.path(), None, &bogus).is_err());

        // Ack by file-set, members in reverse order to prove set (not path) identity.
        let ack = ConstraintsArgs {
            action: "ack-cycle".into(),
            members: Some(vec!["src/b.rs".into(), "src/a.rs".into()]),
            rationale: Some("reviewed: idiomatic re-export cycle".into()),
            acked_by: Some("josh".into()),
            ..blank_args()
        };
        let out = handle_ack_cycle(&db, dir.path(), None, &ack).unwrap();
        assert_eq!(out["acked_cycle"], "builtin:cycles");
        assert_eq!(out["members"], "src/a.rs -> src/b.rs");
        assert_eq!(out["file"], "src/a.rs");

        // Persisted to the file as the reserved builtin key (the sole id-keyed entry).
        let onfile = accepted::load_accepted_file(dir.path()).unwrap();
        assert_eq!(onfile.acks.len(), 1);
        assert_eq!(onfile.acks[0].constraint, "builtin:cycles");
        assert_eq!(
            onfile.acks[0].snippet.as_deref(),
            Some("src/a.rs -> src/b.rs")
        );

        let after = handle_violations(&db, dir.path(), None).unwrap();
        assert_eq!(
            active_cycle_count(&after),
            0,
            "acked cycle drops off report"
        );
        assert_eq!(
            after["acknowledged"].as_array().unwrap().len(),
            1,
            "ack state surfaced, not silent"
        );

        // Reverse it: the cycle comes back.
        let unack = ConstraintsArgs {
            action: "unack-cycle".into(),
            members: Some(vec!["src/a.rs".into(), "src/b.rs".into()]),
            ..blank_args()
        };
        handle_unack_cycle(&db, dir.path(), &unack).unwrap();
        let restored = handle_violations(&db, dir.path(), None).unwrap();
        assert_eq!(active_cycle_count(&restored), 1, "unack restores the cycle");
    }

    /// A cycle's identity is its file *set*: growing the SCC to a new member yields
    /// a different fingerprint, so an ack of the old set no longer cancels it and it
    /// re-surfaces (the whole point over a leaky single-path key, sutra/359).
    #[test]
    fn reshaped_cycle_resurfaces_past_an_ack() {
        let (dir, db) = cycle_workspace("");

        let ack = ConstraintsArgs {
            action: "ack-cycle".into(),
            members: Some(vec!["src/a.rs".into(), "src/b.rs".into()]),
            rationale: Some("reviewed".into()),
            acked_by: Some("josh".into()),
            ..blank_args()
        };
        handle_ack_cycle(&db, dir.path(), None, &ack).unwrap();
        assert_eq!(
            active_cycle_count(&handle_violations(&db, dir.path(), None).unwrap()),
            0
        );

        // Grow the SCC to {a, b, c}: b -> c -> a closes a larger loop.
        db.upsert_file("src/c.rs", "rust", "h3", 10, true).unwrap();
        let fa = db.file_by_path("src/a.rs").unwrap().unwrap();
        let fb = db.file_by_path("src/b.rs").unwrap().unwrap();
        let fc = db.file_by_path("src/c.rs").unwrap().unwrap();
        db.insert_import(fb.id, "src/c.rs", Some(fc.id), 2, "use", None)
            .unwrap();
        db.insert_import(fc.id, "src/a.rs", Some(fa.id), 2, "use", None)
            .unwrap();

        let reshaped = handle_violations(&db, dir.path(), None).unwrap();
        let rc = cycle_finding(&reshaped).expect("reshaped cycle re-surfaces");
        assert_eq!(rc["snippet"], "src/a.rs -> src/b.rs -> src/c.rs");
        assert_eq!(active_cycle_count(&reshaped), 1);
    }

    /// A cycle fully inside an authored, named `no_cycles` rule was only *waivable*
    /// before (leaky from_path); now it is ackable by file-set too, keyed by the
    /// rule's name.
    #[test]
    fn owned_named_cycle_ackable_by_file_set() {
        let rule = "[[constraint]]\nkind = \"no_cycles\"\nname = \"no-module-cycles\"\n";
        let (dir, db) = cycle_workspace(rule);

        let before = handle_violations(&db, dir.path(), None).unwrap();
        let cyc = cycle_finding(&before).expect("owned cycle reported");
        assert_ne!(cyc["constraint_id"], "builtin:cycles", "owned by the rule");

        let ack = ConstraintsArgs {
            action: "ack-cycle".into(),
            members: Some(vec!["src/a.rs".into(), "src/b.rs".into()]),
            rationale: Some("reviewed".into()),
            acked_by: Some("josh".into()),
            ..blank_args()
        };
        let out = handle_ack_cycle(&db, dir.path(), None, &ack).unwrap();
        assert_eq!(out["acked_cycle"], "no-module-cycles");

        let after = handle_violations(&db, dir.path(), None).unwrap();
        assert_eq!(active_cycle_count(&after), 0);
    }

    /// Default-filled args so tests set only the fields they exercise.
    fn blank_args() -> ConstraintsArgs {
        ConstraintsArgs {
            workspace: "test".into(),
            action: String::new(),
            constraint_id: None,
            constraint_name: None,
            file_path: None,
            symbol_qualified_name: None,
            rationale: None,
            waived_by: None,
            acked_by: None,
            line: None,
            snippet: None,
            scope: None,
            members: None,
        }
    }
}
