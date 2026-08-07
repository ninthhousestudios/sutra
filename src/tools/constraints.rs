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
    /// Action: "list", "violations", "waive", "unwaive", "baseline", "ack", "unack"
    pub action: String,
    /// Constraint ID — 8-char blake3 hash (for waive/baseline/ack)
    #[serde(default)]
    pub constraint_id: Option<String>,
    /// Human-readable constraint name (optional; alternative to constraint_id for
    /// baseline/ack, and stored with waiver/ack for display)
    #[serde(default)]
    pub constraint_name: Option<String>,
    /// File path the waiver/ack applies to (for waive/ack)
    #[serde(default)]
    pub file_path: Option<String>,
    /// Symbol qualified name to scope the waiver (optional, for waive)
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
    /// Waiver ID (for unwaive)
    #[serde(default)]
    pub waiver_id: Option<i64>,
    /// Instance-ack ID (for unack)
    #[serde(default)]
    pub ack_id: Option<i64>,
    /// Line of the specific match instance to ack (for ack; or use `snippet`)
    #[serde(default)]
    pub line: Option<u32>,
    /// Snippet (matched node's first line) selecting the instance to ack (for ack)
    #[serde(default)]
    pub snippet: Option<String>,
    /// Glob restricting which files a baseline snapshots (optional, for baseline)
    #[serde(default)]
    pub scope: Option<String>,
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
        "baseline" => handle_baseline(db, workspace_root, dd_engine, args),
        "ack" => handle_ack(db, workspace_root, args),
        "unack" => handle_unack(db, args),
        other => Err(SutraError::Internal(format!(
            "unknown action: {other}. expected: list, violations, waive, unwaive, \
             baseline, ack, unack"
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
    // is visible, not silent (sutra/305).
    let acks = db.get_all_constraint_instance_acks().unwrap_or_default();
    if !acks.is_empty() {
        result["acknowledged"] = json!(acks.iter().map(ack_to_json).collect::<Vec<_>>());
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

fn ack_to_json(a: &crate::db::ConstraintInstanceAckRow) -> serde_json::Value {
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

/// Resolve a constraint by id (preferred) or name from the loaded rules,
/// returning its `(id, name)`. Baseline/ack accept either handle.
fn resolve_constraint(
    workspace_root: &Path,
    constraint_id: Option<&str>,
    constraint_name: Option<&str>,
) -> Result<(String, Option<String>)> {
    let mut rules = rules::load_rules(workspace_root)?;
    let (all_constraints, _errors) = rules.all_constraints();
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
    let (constraint_id, constraint_name) = resolve_constraint(
        workspace_root,
        args.constraint_id.as_deref(),
        args.constraint_name.as_deref(),
    )?;
    let acked_by = args
        .acked_by
        .as_deref()
        .ok_or_else(|| SutraError::Internal("baseline requires acked_by".into()))?;

    let registry = crate::parser::adapter::default_registry();
    let mut rules_loaded = rules::load_rules(workspace_root)?;
    let (all_constraints, _errors) = rules_loaded.all_constraints();

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
    let mut keys_acked = 0usize;
    let mut instances = 0i64;
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
        let count = (j - i) as i64;
        let f = &target[i];
        db.create_constraint_instance_ack(
            &constraint_id,
            constraint_name.as_deref(),
            &f.from_path,
            f.enclosing_symbol.as_deref(),
            f.snippet.as_deref(),
            count,
            args.rationale.as_deref(),
            acked_by,
        )?;
        keys_acked += 1;
        instances += count;
        i = j;
    }

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
    let (constraint_id, constraint_name) = resolve_constraint(
        workspace_root,
        args.constraint_id.as_deref(),
        args.constraint_name.as_deref(),
    )?;
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

    let content = std::fs::read_to_string(workspace_root.join(file_path))
        .map_err(|e| SutraError::Internal(format!("cannot read {file_path}: {e}")))?;
    let registry = crate::parser::adapter::default_registry();
    let mut rules_loaded = rules::load_rules(workspace_root)?;
    let (all_constraints, _errors) = rules_loaded.all_constraints();
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

    let id = db.create_constraint_instance_ack(
        &constraint_id,
        constraint_name.as_deref(),
        file_path,
        selected.enclosing_symbol.as_deref(),
        selected.snippet.as_deref(),
        new_count,
        Some(rationale),
        acked_by,
    )?;

    Ok(json!({
        "ack_id": id,
        "constraint_id": constraint_id,
        "file_path": file_path,
        "enclosing_symbol": selected.enclosing_symbol,
        "snippet": selected.snippet,
        "accepted_count": new_count,
        "matched": matched,
    }))
}

fn handle_unack(db: &Db, args: &ConstraintsArgs) -> Result<serde_json::Value> {
    let ack_id = args
        .ack_id
        .ok_or_else(|| SutraError::Internal("unack requires ack_id".into()))?;
    let deleted = db.delete_constraint_instance_ack(ack_id)?;
    if !deleted {
        return Err(SutraError::Internal(format!("ack {ack_id} not found")));
    }
    Ok(json!({ "revoked_ack": ack_id }))
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

        // unack the foo key -> both foo clones resurface.
        let ack_id = after
            .get("acknowledged")
            .and_then(|a| a.as_array())
            .and_then(|a| a.iter().find(|r| r["snippet"] == "foo.clone()"))
            .map(|r| r["ack_id"].as_i64().unwrap())
            .unwrap();
        let unack = ConstraintsArgs {
            workspace: "test".into(),
            action: "unack".into(),
            ack_id: Some(ack_id),
            ..blank_args()
        };
        handle_unack(&db, &unack).unwrap();
        let restored = handle_violations(&db, dir.path(), None).unwrap();
        assert_eq!(
            active_pattern_count(&restored),
            3,
            "unack restores all matches"
        );
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
            waiver_id: None,
            ack_id: None,
            line: None,
            snippet: None,
            scope: None,
        }
    }
}
