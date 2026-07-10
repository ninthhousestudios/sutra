use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde::Deserialize;

use crate::constraints::check::{self, CheckOutcome, EvalScope, FactsSource};
use crate::parser::ParseResult;

pub const DEFAULT_PAGERANK_MIN: f64 = 0.05;
pub const DEFAULT_BLAST_MIN: i64 = 10;
pub const DEFAULT_ACK_TTL_SECS: u64 = 600;

#[derive(Debug, Clone, Copy)]
pub struct GuardConfig {
    pub pagerank_min: f64,
    pub blast_min: i64,
    pub ack_ttl_secs: u64,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            pagerank_min: DEFAULT_PAGERANK_MIN,
            blast_min: DEFAULT_BLAST_MIN,
            ack_ttl_secs: DEFAULT_ACK_TTL_SECS,
        }
    }
}

impl GuardConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();
        if let Ok(v) = std::env::var("SUTRA_GUARD_PAGERANK_MIN")
            && let Ok(parsed) = v.parse::<f64>()
            && parsed.is_finite()
            && parsed >= 0.0
        {
            cfg.pagerank_min = parsed;
        }
        if let Ok(v) = std::env::var("SUTRA_GUARD_BLAST_MIN")
            && let Ok(parsed) = v.parse::<i64>()
            && parsed >= 0
        {
            cfg.blast_min = parsed;
        }
        if let Ok(v) = std::env::var("SUTRA_GUARD_ACK_TTL_SECS")
            && let Ok(parsed) = v.parse::<u64>()
        {
            cfg.ack_ttl_secs = parsed;
        }
        cfg
    }

    pub fn is_disabled() -> bool {
        std::env::var("SUTRA_GUARD_DISABLE")
            .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false)
    }
}

#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub tool_name: String,
    pub tool_input: ToolInput,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub hook_event_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ToolInput {
    pub file_path: Option<String>,
    pub old_string: Option<String>,
    pub new_string: Option<String>,
    pub content: Option<String>,
}

pub fn is_additive_edit(tool_input: &ToolInput) -> bool {
    match (&tool_input.old_string, &tool_input.new_string) {
        (Some(old), Some(new)) => {
            let old_lines: Vec<&str> = old.lines().collect();
            let new_lines: Vec<&str> = new.lines().collect();
            if old_lines.len() > new_lines.len() {
                return false;
            }
            let mut old_idx = 0;
            for new_line in &new_lines {
                if old_idx < old_lines.len() && *new_line == old_lines[old_idx] {
                    old_idx += 1;
                }
            }
            old_idx == old_lines.len()
        }
        _ => false,
    }
}

#[derive(Debug, Clone)]
pub enum GuardDecision {
    Allow,
    Deny { reason: String },
}

#[derive(Debug, Clone)]
pub struct FileFacts {
    pub rel_path: String,
    pub pagerank: f64,
    pub blast_radius: i64,
    pub hot_symbols: Vec<(String, f64)>,
}

pub fn evaluate(facts: &FileFacts, cfg: &GuardConfig, ack_fresh: bool) -> GuardDecision {
    let hot_pagerank = facts.pagerank >= cfg.pagerank_min;
    let hot_blast = facts.blast_radius >= cfg.blast_min;
    if !hot_pagerank && !hot_blast {
        return GuardDecision::Allow;
    }
    if ack_fresh {
        return GuardDecision::Allow;
    }
    GuardDecision::Deny {
        reason: format_deny_reason(facts, cfg, hot_pagerank, hot_blast),
    }
}

fn format_deny_reason(
    facts: &FileFacts,
    cfg: &GuardConfig,
    hot_pagerank: bool,
    hot_blast: bool,
) -> String {
    let mut triggers: Vec<String> = Vec::new();
    if hot_pagerank {
        triggers.push(format!(
            "PageRank {:.4} >= {:.4}",
            facts.pagerank, cfg.pagerank_min
        ));
    }
    if hot_blast {
        triggers.push(format!(
            "blast radius {} >= {}",
            facts.blast_radius, cfg.blast_min
        ));
    }

    let top_sym = facts
        .hot_symbols
        .first()
        .map(|(name, _)| name.as_str())
        .unwrap_or(&facts.rel_path);

    let mut reason = format!(
        "STOP: `{}` is load-bearing ({}). Call `sutra_impact` with \
         symbol=\"{}\" FIRST to review direct and transitive importers, \
         then retry the edit. Opt out: `SUTRA_GUARD_DISABLE=1`.",
        facts.rel_path,
        triggers.join(", "),
        top_sym,
    );
    if !facts.hot_symbols.is_empty() {
        let parts: Vec<String> = facts
            .hot_symbols
            .iter()
            .map(|(name, rank)| format!("{name} (pr={rank:.3})"))
            .collect();
        reason.push_str(" Hot symbols in this file: ");
        reason.push_str(&parts.join(", "));
        reason.push('.');
    }
    reason
}

pub fn render_stdout(decision: &GuardDecision, event_name: Option<&str>) -> Option<String> {
    match decision {
        GuardDecision::Allow => None,
        GuardDecision::Deny { reason } => {
            if event_name == Some("BeforeTool") {
                Some(serde_json::json!({ "decision": "deny", "reason": reason }).to_string())
            } else {
                Some(
                    serde_json::json!({
                        "hookSpecificOutput": {
                            "hookEventName": "PreToolUse",
                            "permissionDecision": "deny",
                            "permissionDecisionReason": reason,
                        }
                    })
                    .to_string(),
                )
            }
        }
    }
}

pub fn workspace_id_from_path(root: &Path) -> String {
    root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_lowercase()
        .replace(' ', "-")
}

pub fn sutra_db_dir() -> PathBuf {
    std::env::var("SUTRA_DB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".sutra")
        })
}

fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

pub fn ack_path(project_root: &Path, rel_path: &str) -> PathBuf {
    let digest = format!("{:016x}", fnv1a_64(rel_path.as_bytes()));
    project_root.join(".sutra").join("acks").join(digest)
}

pub fn touch_ack(project_root: &Path, rel_path: &str) {
    let path = ack_path(project_root, rel_path);
    if let Some(parent) = path.parent()
        && std::fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = std::fs::write(&path, ts.to_string());
}

pub fn ack_is_fresh(project_root: &Path, rel_path: &str, ttl_secs: u64) -> bool {
    let path = ack_path(project_root, rel_path);
    let Ok(meta) = std::fs::metadata(&path) else {
        return false;
    };
    let Ok(mtime) = meta.modified() else {
        return false;
    };
    let Ok(elapsed) = SystemTime::now().duration_since(mtime) else {
        return false;
    };
    elapsed < Duration::from_secs(ttl_secs)
}

pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    for _ in 0..20 {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
    None
}

pub fn relativize_file_path(project_root: &Path, file_path: &Path) -> Option<String> {
    let canonical_root = project_root.canonicalize().ok()?;
    let canonical_file = file_path
        .canonicalize()
        .ok()
        .unwrap_or_else(|| file_path.to_path_buf());
    canonical_file
        .strip_prefix(&canonical_root)
        .ok()
        .or_else(|| file_path.strip_prefix(project_root).ok())
        .map(|p| p.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------
// Constraint checking (lightweight, per-edit)
// ---------------------------------------------------------------------------

pub use crate::constraints::ConstraintFinding;

pub fn build_proposed_content(
    tool_input: &ToolInput,
    project_root: &Path,
    rel_path: &str,
) -> Option<String> {
    match (&tool_input.old_string, &tool_input.new_string) {
        (Some(old), Some(new)) => {
            let abs_path = project_root.join(rel_path);
            let current = std::fs::read_to_string(&abs_path).ok()?;
            let proposed = current.replacen(old.as_str(), new.as_str(), 1);
            if proposed == current {
                None
            } else {
                Some(proposed)
            }
        }
        _ => tool_input.content.clone(),
    }
}

fn language_from_path(path: &str) -> Option<&'static str> {
    if path.ends_with(".rs") {
        Some("rust")
    } else if path.ends_with(".dart") {
        Some("dart")
    } else {
        None
    }
}

pub fn parse_proposed(rel_path: &str, proposed_content: &str) -> Option<ParseResult> {
    let language = language_from_path(rel_path)?;
    let result = crate::parser::parse_file(proposed_content, language, rel_path).ok()?;
    if result.parsed_ok { Some(result) } else { None }
}

pub fn is_signature_preserving(conn: &Connection, file_id: i64, result: &ParseResult) -> bool {
    type Key = (String, String, Option<String>, Option<String>);

    let mut proposed_keys: Vec<Key> = crate::parser::flatten_symbols(&result.symbols)
        .into_iter()
        .map(|s| {
            (
                s.qualified_name.clone(),
                s.kind.as_str().to_string(),
                s.signature_hash.clone(),
                s.visibility.clone(),
            )
        })
        .collect();
    proposed_keys.sort();

    let mut stmt = match conn.prepare(
        "SELECT qualified_name, kind, signature_hash, visibility FROM symbols WHERE file_id = ?1",
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let mut indexed_keys: Vec<Key> = match stmt.query_map(params![file_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    }) {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => return false,
    };
    indexed_keys.sort();

    for indexed in &indexed_keys {
        if !proposed_keys.contains(indexed) {
            return false;
        }
    }
    true
}

/// Imports extracted from proposed (not-yet-written) file content.
pub struct ProposedImports {
    /// Workspace-internal resolved edges. For languages without proposed-edge
    /// support (dart), falls back to the file's currently indexed outgoing edges.
    pub edges: Vec<(i64, i64)>,
    /// External `(from_path, crate_name)` imports for external-crate constraints.
    pub externals: Vec<(String, String)>,
}

pub fn extract_proposed_imports(
    conn: &Connection,
    project_root: &Path,
    rel_path: &str,
    file_id: i64,
    result: &ParseResult,
) -> Option<ProposedImports> {
    let language = language_from_path(rel_path)?;

    let layout = if language == "rust" {
        Some(crate::rust_imports::parse_workspace_layout(project_root))
    } else {
        None
    };
    let crate_names: Vec<&str> = layout
        .as_ref()
        .map(|l| l.all_crate_names())
        .unwrap_or_default();

    let mut externals = Vec::new();
    for import in &result.imports {
        if let Some(name) = crate::constraints::external::external_crate_of_import(
            &import.raw_path,
            language,
            &crate_names,
        ) {
            externals.push((rel_path.to_string(), name));
        }
    }

    let edges = if language == "rust" {
        let layout = layout.as_ref().unwrap();
        let path_to_id: HashMap<String, i64> = conn
            .prepare("SELECT path, id FROM files")
            .ok()?
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();
        let path_ref_map: HashMap<&str, i64> =
            path_to_id.iter().map(|(k, v)| (k.as_str(), *v)).collect();

        let mut edges = Vec::new();
        for import in &result.imports {
            let resolved = match crate::rust_imports::normalize_to_crate_segments(
                &import.raw_path,
                rel_path,
                layout,
            ) {
                Some(r) if !r.segments.is_empty() => r,
                _ => continue,
            };
            if let Some(target_id) = crate::rust_imports::resolve_segments(
                &resolved.segments,
                &path_ref_map,
                &resolved.src_prefix,
            ) && target_id != file_id
            {
                edges.push((file_id, target_id));
            }
        }
        edges
    } else {
        // dart: proposed-edge derivation unsupported — use indexed outgoing edges
        conn.prepare(
            "SELECT file_id, resolved_file_id FROM imports \
             WHERE file_id = ?1 AND resolved_file_id IS NOT NULL",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![file_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .unwrap_or_default()
    };

    Some(ProposedImports { edges, externals })
}

fn get_incoming_edges(conn: &Connection, file_id: i64) -> Vec<(i64, i64)> {
    conn.prepare(
        "SELECT file_id, resolved_file_id FROM imports \
         WHERE resolved_file_id = ?1 AND resolved_file_id IS NOT NULL",
    )
    .and_then(|mut stmt| {
        stmt.query_map(params![file_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    })
    .unwrap_or_default()
}

pub fn check_proposed_file_constraints(
    conn: &Connection,
    project_root: &Path,
    file_id: i64,
    proposed_outgoing: &[(i64, i64)],
    proposed_externals: &[(String, String)],
) -> CheckOutcome {
    let registry = crate::parser::adapter::default_registry();
    let mut edges: Vec<(i64, i64)> = proposed_outgoing.to_vec();
    edges.extend(get_incoming_edges(conn, file_id));
    check::evaluate(
        &FactsSource::RawConn(conn),
        project_root,
        EvalScope::Edges {
            edges: &edges,
            externals: proposed_externals,
        },
        &registry,
    )
    .unwrap_or_default()
}

/// Check a proposed Cargo.toml against external-crate constraints.
/// Manifests aren't indexed files, so this runs outside the edge machinery.
pub fn check_proposed_manifest(
    conn: &Connection,
    project_root: &Path,
    manifest_rel_path: &str,
    proposed_content: &str,
) -> CheckOutcome {
    check::check_manifest_raw(conn, project_root, manifest_rel_path, proposed_content)
        .unwrap_or_default()
}

/// Check a proposed pubspec.yaml against external-crate constraints.
pub fn check_proposed_pubspec(
    conn: &Connection,
    project_root: &Path,
    pubspec_rel_path: &str,
    proposed_content: &str,
) -> CheckOutcome {
    check::check_pubspec_raw(conn, project_root, pubspec_rel_path, proposed_content)
        .unwrap_or_default()
}

pub fn check_file_constraints(
    conn: &Connection,
    project_root: &Path,
    file_id: i64,
) -> CheckOutcome {
    let registry = crate::parser::adapter::default_registry();
    check::evaluate(
        &FactsSource::RawConn(conn),
        project_root,
        EvalScope::SingleFile(file_id),
        &registry,
    )
    .unwrap_or_default()
}

pub fn format_constraint_deny(findings: &[&ConstraintFinding]) -> String {
    let mut reason = String::from("STOP: blocking constraint violation(s) detected. ");
    for (i, f) in findings.iter().enumerate() {
        if i > 0 {
            reason.push_str(" | ");
        }
        if let Some(name) = &f.constraint_name {
            reason.push_str(&format!("[{}] {}", name, f.detail));
        } else {
            reason.push_str(&f.detail);
        }
    }
    reason.push_str(". Run `sutra_review` for full details.");
    reason
}

/// Check proposed content against forbidden_pattern constraints using
/// introduced-only semantics: deny only if the match count increased
/// compared to the on-disk version. Waived symbols are excluded from
/// both counts.
pub fn check_proposed_patterns(
    conn: &Connection,
    project_root: &Path,
    rel_path: &str,
    proposed_content: &str,
) -> CheckOutcome {
    use crate::constraints::patterns::check_forbidden_patterns;
    use crate::db::ConstraintWaiverRow;
    use crate::rules::{self, ConstraintKind};
    use crate::waivers;
    use std::sync::Arc;

    let registry = crate::parser::adapter::default_registry();

    let mut loaded_rules = match rules::load_rules(project_root) {
        Ok(r) => r,
        Err(_) => return CheckOutcome::default(),
    };
    let (all_constraints, parse_errors) = loaded_rules.all_constraints();

    let has_patterns = all_constraints
        .iter()
        .any(|c| matches!(c.kind, ConstraintKind::ForbiddenPattern { .. }));
    if !has_patterns {
        return CheckOutcome {
            parse_errors,
            ..Default::default()
        };
    }

    let proposed_findings =
        check_forbidden_patterns(&all_constraints, &[(rel_path, proposed_content)], &registry);
    if proposed_findings.is_empty() {
        return CheckOutcome {
            parse_errors,
            ..Default::default()
        };
    }

    let constraint_waivers: Vec<ConstraintWaiverRow> = conn
        .prepare(
            "SELECT id, constraint_id, constraint_name, file_path, \
             symbol_qualified_name, rationale, waived_by, created_at, updated_at \
             FROM constraint_waivers WHERE file_path = ?1",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![rel_path], |row| {
                Ok(ConstraintWaiverRow {
                    id: row.get(0)?,
                    constraint_id: Arc::from(row.get::<_, String>(1)?),
                    constraint_name: row.get::<_, Option<String>>(2)?.map(Arc::from),
                    file_path: row.get(3)?,
                    symbol_qualified_name: row.get(4)?,
                    rationale: row.get(5)?,
                    waived_by: row.get(6)?,
                    created_at: row.get(7)?,
                    updated_at: row.get(8)?,
                })
            })?
            .collect()
        })
        .unwrap_or_default();

    let (proposed_active, proposed_waived) =
        waivers::partition(proposed_findings, &constraint_waivers);

    if proposed_active.is_empty() {
        return CheckOutcome {
            waived: proposed_waived,
            parse_errors,
            ..Default::default()
        };
    }

    let disk_content = std::fs::read_to_string(project_root.join(rel_path)).unwrap_or_default();
    let disk_findings =
        check_forbidden_patterns(&all_constraints, &[(rel_path, &disk_content)], &registry);
    let (disk_active, _) = waivers::partition(disk_findings, &constraint_waivers);

    // Multiset diff by (constraint_id, enclosing_symbol, snippet) — each disk
    // match cancels one proposed match with the same key. What remains is introduced.
    type MatchKey = (Arc<str>, Option<String>, Option<String>);
    let mut disk_multiset: HashMap<MatchKey, usize> = HashMap::new();
    for f in &disk_active {
        let key: MatchKey = (
            f.constraint_id.clone(),
            f.enclosing_symbol.clone(),
            f.snippet.clone(),
        );
        *disk_multiset.entry(key).or_default() += 1;
    }

    let mut introduced = Vec::new();
    for f in proposed_active {
        let key: MatchKey = (
            f.constraint_id.clone(),
            f.enclosing_symbol.clone(),
            f.snippet.clone(),
        );
        if let Some(count) = disk_multiset.get_mut(&key)
            && *count > 0
        {
            *count -= 1;
            continue;
        }
        introduced.push(f);
    }

    CheckOutcome {
        active: introduced,
        waived: proposed_waived,
        parse_errors,
        ..Default::default()
    }
}

pub fn format_pattern_deny(findings: &[&ConstraintFinding]) -> String {
    let mut reason = format!(
        "STOP: {} new forbidden-pattern match(es) introduced. ",
        findings.len()
    );
    for (i, f) in findings.iter().enumerate() {
        if i > 0 {
            reason.push_str(" | ");
        }
        let name = f.constraint_name.as_deref().unwrap_or("forbidden_pattern");
        let provenance = f
            .provenance
            .as_deref()
            .map(|p| format!(" ({p})"))
            .unwrap_or_default();
        reason.push_str(&format!(
            "{name}{provenance} at {}{}",
            f.from_path,
            f.line.map(|l| format!(":{l}")).unwrap_or_default(),
        ));
        if let Some(snippet) = &f.snippet {
            reason.push_str(&format!(": {snippet}"));
        }
    }
    reason.push_str(
        ". If this use is intentional and justified, waive for this symbol via \
         `sutra_constraints action=waive` with a rationale explaining why. \
         Otherwise restructure to avoid the pattern.",
    );
    reason
}

// ---------------------------------------------------------------------------
// Install / uninstall
// ---------------------------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from)
}

fn claude_settings_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let home = home_dir().ok_or("cannot determine home directory")?;
    Ok(home.join(".claude").join("settings.json"))
}

fn find_guard_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let candidates = [
        home_dir().map(|h| h.join(".cargo/bin/sutra-guard")),
        Some(PathBuf::from("/usr/local/bin/sutra-guard")),
        home_dir().map(|h| h.join(".local/bin/sutra-guard")),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    if let Ok(output) = std::process::Command::new("which")
        .arg("sutra-guard")
        .output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    Err("sutra-guard binary not found. Run: cargo install --path . --bin sutra-guard".into())
}

pub fn install() -> Result<(), Box<dyn std::error::Error>> {
    let guard_bin = find_guard_binary()?;
    let settings_path = claude_settings_path()?;

    let mut settings = if settings_path.exists() {
        let raw = std::fs::read_to_string(&settings_path)?;
        serde_json::from_str::<serde_json::Value>(&raw)?
    } else {
        serde_json::json!({})
    };

    let hooks = settings
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .unwrap();

    for event in &["PreToolUse", "SessionStart"] {
        if let Some(arr) = hooks.get_mut(*event).and_then(|v| v.as_array_mut()) {
            arr.retain(|entry| {
                let cmd = entry
                    .pointer("/hooks/0/command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                !cmd.contains("qartez")
            });
        }
    }

    let guard_str = guard_bin.to_string_lossy().to_string();

    let routing_hook = serde_json::json!({
        "matcher": "Glob|Grep",
        "hooks": [{ "type": "command", "command": &guard_str, "timeout": 3000 }]
    });
    let mod_hook = serde_json::json!({
        "matcher": "Edit|Write|MultiEdit",
        "hooks": [{ "type": "command", "command": &guard_str, "timeout": 3000 }]
    });

    let pre_tool = hooks
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .unwrap();

    pre_tool.retain(|entry| {
        let cmd = entry
            .pointer("/hooks/0/command")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        !cmd.contains("sutra-guard")
    });

    pre_tool.push(routing_hook);
    pre_tool.push(mod_hook);

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;

    println!("Installed sutra-guard hooks to {}", settings_path.display());
    println!("Guard binary: {guard_str}");
    println!("Removed any existing qartez hooks.");
    Ok(())
}

pub fn uninstall() -> Result<(), Box<dyn std::error::Error>> {
    let settings_path = claude_settings_path()?;
    if !settings_path.exists() {
        println!("No settings file found at {}", settings_path.display());
        return Ok(());
    }

    let raw = std::fs::read_to_string(&settings_path)?;
    let mut settings: serde_json::Value = serde_json::from_str(&raw)?;

    if let Some(hooks) = settings.pointer_mut("/hooks")
        && let Some(pre_tool) = hooks
            .pointer_mut("/PreToolUse")
            .and_then(|v| v.as_array_mut())
    {
        pre_tool.retain(|entry| {
            let cmd = entry
                .pointer("/hooks/0/command")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            !cmd.contains("sutra-guard")
        });
    }

    std::fs::write(&settings_path, serde_json::to_string_pretty(&settings)?)?;
    println!("Removed sutra-guard hooks from {}", settings_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constraints::FindingDelta;
    use crate::rules::Severity;

    #[test]
    fn additive_append() {
        let input = ToolInput {
            file_path: Some("src/lib.rs".into()),
            old_string: Some("pub mod rest;\npub mod rules;".into()),
            new_string: Some("pub mod rest;\npub mod rules;\npub mod tools;".into()),
            content: None,
        };
        assert!(is_additive_edit(&input));
    }

    #[test]
    fn modification_not_additive() {
        let input = ToolInput {
            file_path: Some("src/lib.rs".into()),
            old_string: Some("pub mod rest;".into()),
            new_string: Some("pub mod router;".into()),
            content: None,
        };
        assert!(!is_additive_edit(&input));
    }

    #[test]
    fn write_tool_not_additive() {
        let input = ToolInput {
            file_path: Some("src/lib.rs".into()),
            old_string: None,
            new_string: None,
            content: Some("full file content".into()),
        };
        assert!(!is_additive_edit(&input));
    }

    #[test]
    fn pure_suffix_append() {
        let input = ToolInput {
            file_path: Some("src/lib.rs".into()),
            old_string: Some("use foo;\n".into()),
            new_string: Some("use foo;\nuse bar;\n".into()),
            content: None,
        };
        assert!(is_additive_edit(&input));
    }

    fn make_finding(severity: Severity) -> ConstraintFinding {
        ConstraintFinding {
            constraint_id: "abc12345".into(),
            constraint_name: Some("no-tools-daemon".into()),
            constraint_kind: "forbidden_dep".into(),
            severity,
            provenance: None,
            from_path: "src/tools/foo.rs".into(),
            to_path: "src/daemon.rs".into(),
            component_context: None,
            detail:
                "forbidden: src/tools/foo.rs -> src/daemon.rs (rule: src/tools/* -> src/daemon.rs)"
                    .into(),
            delta: FindingDelta::Unknown,
            line: None,
            snippet: None,
            enclosing_symbol: None,
        }
    }

    #[test]
    fn blocking_active_produces_deny() {
        let outcome = CheckOutcome {
            active: vec![make_finding(Severity::Blocking)],
            ..Default::default()
        };
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert_eq!(blocking.len(), 1);
        let reason = format_constraint_deny(&blocking);
        assert!(reason.contains("STOP"));
        assert!(reason.contains("no-tools-daemon"));
    }

    #[test]
    fn advisory_does_not_block() {
        let outcome = CheckOutcome {
            active: vec![make_finding(Severity::Advisory)],
            ..Default::default()
        };
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert!(blocking.is_empty());
    }

    #[test]
    fn informational_does_not_block() {
        let outcome = CheckOutcome {
            active: vec![make_finding(Severity::Informational)],
            ..Default::default()
        };
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert!(blocking.is_empty());
    }

    #[test]
    fn waived_blocking_does_not_block() {
        use crate::waivers::Waived;
        let outcome = CheckOutcome {
            waived: vec![Waived {
                finding: make_finding(Severity::Blocking),
                rationale: "accepted".into(),
                waived_by: "josh".into(),
            }],
            ..Default::default()
        };
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert!(blocking.is_empty());
    }

    #[test]
    fn mixed_severities_only_blocking_blocks() {
        use crate::waivers::Waived;
        let outcome = CheckOutcome {
            active: vec![
                make_finding(Severity::Blocking),
                make_finding(Severity::Advisory),
                make_finding(Severity::Informational),
            ],
            waived: vec![Waived {
                finding: make_finding(Severity::Blocking),
                rationale: "accepted".into(),
                waived_by: "josh".into(),
            }],
            ..Default::default()
        };
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert_eq!(blocking.len(), 1);
        let advisory: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity != Severity::Blocking)
            .collect();
        assert_eq!(advisory.len(), 2);
    }

    #[test]
    fn check_file_constraints_empty_without_rules() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');",
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let outcome = check_file_constraints(&conn, dir.path(), 1);
        assert!(outcome.active.is_empty());
    }

    #[test]
    fn check_file_constraints_finds_violation() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             INSERT INTO files VALUES (1, 'src/tools/review.rs');
             INSERT INTO files VALUES (2, 'src/daemon.rs');
             INSERT INTO imports VALUES (1, 2);",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/tools/*"
to = "src/daemon.rs"
severity = "blocking"
name = "no-tools-daemon"
"#,
        )
        .unwrap();

        let outcome = check_file_constraints(&conn, dir.path(), 1);
        assert_eq!(outcome.active.len(), 1);
        assert_eq!(outcome.active[0].severity, Severity::Blocking);
        assert_eq!(
            outcome.active[0].constraint_name.as_deref(),
            Some("no-tools-daemon")
        );
    }

    #[test]
    fn check_file_constraints_respects_waivers() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             INSERT INTO files VALUES (1, 'src/tools/review.rs');
             INSERT INTO files VALUES (2, 'src/daemon.rs');
             INSERT INTO imports VALUES (1, 2);",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/tools/*"
to = "src/daemon.rs"
severity = "blocking"
name = "no-tools-daemon"
"#,
        )
        .unwrap();

        // First, get the constraint ID by running without waivers
        let outcome = check_file_constraints(&conn, dir.path(), 1);
        assert_eq!(outcome.active.len(), 1);
        let constraint_id = outcome.active[0].constraint_id.clone();

        // Now add waiver
        conn.execute(
            "INSERT INTO constraint_waivers (constraint_id, file_path, rationale, waived_by) VALUES (?1, 'src/tools/review.rs', 'accepted', 'test')",
            params![constraint_id],
        )
        .unwrap();

        let outcome = check_file_constraints(&conn, dir.path(), 1);
        assert!(outcome.active.is_empty());
        assert_eq!(outcome.waived.len(), 1);
    }

    #[test]
    fn check_file_constraints_advisory_severity() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             INSERT INTO files VALUES (1, 'src/config.rs');
             INSERT INTO files VALUES (2, 'src/db/mod.rs');
             INSERT INTO imports VALUES (2, 1);",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/db/*"
to = "src/config.rs"
severity = "advisory"
name = "db-config-coupling"
"#,
        )
        .unwrap();

        // Query from file_id=1 (config.rs) — should find the incoming edge from db/mod.rs
        let outcome = check_file_constraints(&conn, dir.path(), 1);
        assert_eq!(outcome.active.len(), 1);
        assert_eq!(outcome.active[0].severity, Severity::Advisory);
    }

    fn setup_constraint_db() -> (Connection, tempfile::TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             INSERT INTO files VALUES (1, 'src/tools/review.rs');
             INSERT INTO files VALUES (2, 'src/daemon.rs');
             INSERT INTO files VALUES (3, 'src/lib.rs');",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_dep"
from = "src/tools/*"
to = "src/daemon.rs"
severity = "blocking"
name = "no-tools-daemon"
"#,
        )
        .unwrap();

        (conn, dir)
    }

    #[test]
    fn proposed_violating_edit_denied() {
        let (conn, dir) = setup_constraint_db();
        let proposed_outgoing = vec![(1, 2)];
        let outcome =
            check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing, &[]);
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert_eq!(blocking.len(), 1);
        assert_eq!(
            blocking[0].constraint_name.as_deref(),
            Some("no-tools-daemon")
        );
    }

    #[test]
    fn proposed_fixing_edit_allowed() {
        let (conn, dir) = setup_constraint_db();
        conn.execute_batch("INSERT INTO imports VALUES (1, 2);")
            .unwrap();
        let proposed_outgoing = vec![(1, 3)];
        let outcome =
            check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing, &[]);
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert!(blocking.is_empty());
    }

    #[test]
    fn proposed_unrelated_edit_to_violating_file_denied() {
        let (conn, dir) = setup_constraint_db();
        let proposed_outgoing = vec![(1, 2), (1, 3)];
        let outcome =
            check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing, &[]);
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert_eq!(blocking.len(), 1);
    }

    #[test]
    fn waiver_on_target_does_not_suppress() {
        let (conn, dir) = setup_constraint_db();
        let proposed_outgoing = vec![(1, 2)];
        let outcome =
            check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing, &[]);
        assert_eq!(outcome.active.len(), 1);
        let constraint_id = outcome.active[0].constraint_id.clone();

        // Waiver on the TARGET file (daemon.rs), not the source
        conn.execute(
            "INSERT INTO constraint_waivers (constraint_id, file_path, rationale, waived_by) VALUES (?1, 'src/daemon.rs', 'accepted', 'test')",
            params![constraint_id],
        )
        .unwrap();

        let outcome =
            check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing, &[]);
        assert_eq!(
            outcome.active.len(),
            1,
            "waiver on target should not suppress with from-only rule"
        );
        assert!(outcome.waived.is_empty());
    }

    #[test]
    fn waiver_on_source_does_suppress() {
        let (conn, dir) = setup_constraint_db();
        let proposed_outgoing = vec![(1, 2)];
        let outcome =
            check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing, &[]);
        assert_eq!(outcome.active.len(), 1);
        let constraint_id = outcome.active[0].constraint_id.clone();

        // Waiver on the SOURCE file (tools/review.rs)
        conn.execute(
            "INSERT INTO constraint_waivers (constraint_id, file_path, rationale, waived_by) VALUES (?1, 'src/tools/review.rs', 'accepted', 'test')",
            params![constraint_id],
        )
        .unwrap();

        let outcome =
            check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing, &[]);
        assert!(outcome.active.is_empty());
        assert_eq!(outcome.waived.len(), 1);
    }

    // --- external-crate constraints ---

    fn setup_external_db() -> (Connection, tempfile::TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, imported_path TEXT, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, language TEXT);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             INSERT INTO files VALUES (1, 'report/src/lib.rs', 'rust');
             INSERT INTO files VALUES (2, 'server/src/main.rs', 'rust');",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_external"
from = "report/**"
crates = ["axum", "sqlx"]
name = "report-stays-pure"

[[constraint]]
kind = "confined_external"
crates = ["tonic", "prost"]
allowed_in = ["quiver-client/**"]
name = "protos-confined"
"#,
        )
        .unwrap();

        (conn, dir)
    }

    #[test]
    fn proposed_external_import_denied() {
        let (conn, dir) = setup_external_db();
        let externals = vec![("report/src/lib.rs".to_string(), "axum".to_string())];
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &[], &externals);
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert_eq!(blocking.len(), 1);
        assert_eq!(
            blocking[0].constraint_name.as_deref(),
            Some("report-stays-pure")
        );
        assert_eq!(blocking[0].to_path, "crate:axum");
    }

    #[test]
    fn proposed_confined_external_denied_outside_allowed_paths() {
        let (conn, dir) = setup_external_db();
        let externals = vec![("server/src/main.rs".to_string(), "tonic".to_string())];
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 2, &[], &externals);
        assert_eq!(outcome.active.len(), 1);
        assert_eq!(
            outcome.active[0].constraint_name.as_deref(),
            Some("protos-confined")
        );

        let allowed = vec![("quiver-client/src/lib.rs".to_string(), "tonic".to_string())];
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 2, &[], &allowed);
        assert!(outcome.active.is_empty());
    }

    #[test]
    fn indexed_unresolved_external_found_in_single_file_check() {
        let (conn, dir) = setup_external_db();
        conn.execute_batch("INSERT INTO imports VALUES (1, 'sqlx::query', NULL);")
            .unwrap();
        let outcome = check_file_constraints(&conn, dir.path(), 1);
        assert_eq!(outcome.active.len(), 1);
        assert_eq!(outcome.active[0].to_path, "crate:sqlx");
    }

    #[test]
    fn external_waiver_suppresses() {
        let (conn, dir) = setup_external_db();
        let externals = vec![("report/src/lib.rs".to_string(), "axum".to_string())];
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &[], &externals);
        let constraint_id = outcome.active[0].constraint_id.clone();

        conn.execute(
            "INSERT INTO constraint_waivers (constraint_id, file_path, rationale, waived_by) VALUES (?1, 'report/src/lib.rs', 'transition', 'test')",
            params![constraint_id],
        )
        .unwrap();

        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &[], &externals);
        assert!(outcome.active.is_empty());
        assert_eq!(outcome.waived.len(), 1);
    }

    #[test]
    fn proposed_manifest_dep_denied() {
        let (conn, dir) = setup_external_db();
        let manifest = "[dependencies]\ntonic = \"0.12\"\n";
        let outcome = check_proposed_manifest(&conn, dir.path(), "server/Cargo.toml", manifest);
        assert_eq!(outcome.active.len(), 1);
        assert!(outcome.active[0].detail.contains("manifest dependency"));

        let outcome =
            check_proposed_manifest(&conn, dir.path(), "quiver-client/Cargo.toml", manifest);
        assert!(outcome.active.is_empty());
    }

    #[test]
    fn external_targeting_member_surfaces_as_blocking_finding() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, imported_path TEXT, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, language TEXT);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             INSERT INTO files VALUES (1, 'server/src/main.rs', 'rust');",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_external"
from = "server/**"
crates = ["report"]
name = "bad-rule-targets-member"
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"report\"]\n",
        )
        .unwrap();
        let member_dir = dir.path().join("report");
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(
            member_dir.join("Cargo.toml"),
            "[package]\nname = \"report\"\n",
        )
        .unwrap();

        // Edges path (check_proposed_file_constraints)
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &[], &[]);
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert_eq!(blocking.len(), 1);
        assert!(blocking[0].detail.contains("bad-rule-targets-member"));
        assert!(blocking[0].detail.contains("forbidden_dep"));

        // SingleFile path (check_file_constraints)
        let outcome = check_file_constraints(&conn, dir.path(), 1);
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert_eq!(blocking.len(), 1);
        assert!(blocking[0].detail.contains("bad-rule-targets-member"));
    }

    #[test]
    fn extract_proposed_imports_separates_internal_and_external() {
        let (conn, dir) = setup_external_db();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"report\"\n",
        )
        .unwrap();
        let content = "use axum::Router;\nuse crate::render;\nuse std::fmt;\n\nfn f() {}\n";
        let parsed = parse_proposed("report/src/lib.rs", content).unwrap();
        let pi =
            extract_proposed_imports(&conn, dir.path(), "report/src/lib.rs", 1, &parsed).unwrap();
        let crates: Vec<&str> = pi.externals.iter().map(|(_, c)| c.as_str()).collect();
        assert!(crates.contains(&"axum"));
        assert!(crates.contains(&"std"));
        assert!(!crates.iter().any(|c| *c == "crate" || *c == "render"));
    }

    #[test]
    fn build_proposed_content_edit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src").join("foo.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "use crate::bar;\nfn main() {}\n").unwrap();

        let input = ToolInput {
            file_path: Some(file.to_string_lossy().into()),
            old_string: Some("use crate::bar;".into()),
            new_string: Some("use crate::baz;".into()),
            content: None,
        };
        let result = build_proposed_content(&input, dir.path(), "src/foo.rs");
        assert_eq!(result.unwrap(), "use crate::baz;\nfn main() {}\n");
    }

    #[test]
    fn build_proposed_content_write() {
        let dir = tempfile::tempdir().unwrap();
        let input = ToolInput {
            file_path: Some("src/foo.rs".into()),
            old_string: None,
            new_string: None,
            content: Some("use crate::new_mod;\nfn main() {}\n".into()),
        };
        let result = build_proposed_content(&input, dir.path(), "src/foo.rs");
        assert_eq!(result.unwrap(), "use crate::new_mod;\nfn main() {}\n");
    }

    #[test]
    fn proposed_incoming_edge_still_checked() {
        let (conn, dir) = setup_constraint_db();
        // daemon.rs (file_id=2) has an incoming edge from tools/review.rs that violates
        conn.execute_batch("INSERT INTO imports VALUES (1, 2);")
            .unwrap();
        // We're editing daemon.rs — proposed outgoing is clean, but incoming is violating
        let proposed_outgoing: Vec<(i64, i64)> = vec![];
        let outcome =
            check_proposed_file_constraints(&conn, dir.path(), 2, &proposed_outgoing, &[]);
        let blocking: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.severity == Severity::Blocking)
            .collect();
        assert_eq!(
            blocking.len(),
            1,
            "incoming violating edge should still block"
        );
    }

    // --- max_fan_in constraints ---

    #[test]
    fn max_fan_in_violation_detected() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, fan_in_files INTEGER DEFAULT 0);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             INSERT INTO files VALUES (1, 'src/config.rs', 15);
             INSERT INTO files VALUES (2, 'src/lib.rs', 3);",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "max_fan_in"
target = "src/config.rs"
threshold = 10
name = "config-fan-in"
"#,
        )
        .unwrap();

        let outcome = check_file_constraints(&conn, dir.path(), 1);
        assert_eq!(outcome.active.len(), 1);
        assert_eq!(outcome.active[0].constraint_kind, "max_fan_in");
        assert_eq!(outcome.active[0].severity, Severity::Advisory);
        assert!(outcome.active[0].detail.contains("fan-in is 15"));
        assert!(outcome.active[0].detail.contains("threshold is 10"));
    }

    #[test]
    fn max_fan_in_below_threshold_no_finding() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, fan_in_files INTEGER DEFAULT 0);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             INSERT INTO files VALUES (1, 'src/config.rs', 5);",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "max_fan_in"
target = "src/config.rs"
threshold = 10
"#,
        )
        .unwrap();

        let outcome = check_file_constraints(&conn, dir.path(), 1);
        assert!(outcome.active.is_empty());
    }

    #[test]
    fn max_fan_in_glob_target() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, fan_in_files INTEGER DEFAULT 0);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             INSERT INTO files VALUES (1, 'src/core/config.rs', 20);
             INSERT INTO files VALUES (2, 'src/core/utils.rs', 5);",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "max_fan_in"
target = "src/core/*"
threshold = 10
"#,
        )
        .unwrap();

        // File 1 matches glob and exceeds threshold
        let outcome = check_file_constraints(&conn, dir.path(), 1);
        assert_eq!(outcome.active.len(), 1);

        // File 2 matches glob but is below threshold
        let outcome = check_file_constraints(&conn, dir.path(), 2);
        assert!(outcome.active.is_empty());
    }

    #[test]
    fn max_fan_in_proposed_edge_to_over_threshold_target() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, fan_in_files INTEGER DEFAULT 0);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');
             INSERT INTO files VALUES (1, 'src/editor.rs', 0);
             INSERT INTO files VALUES (2, 'src/config.rs', 15);",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "max_fan_in"
target = "src/config.rs"
threshold = 10
name = "config-fan-in"
"#,
        )
        .unwrap();

        // Proposed edge from file 1 to file 2 (target already over threshold)
        let proposed_outgoing = vec![(1, 2)];
        let outcome =
            check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing, &[]);
        assert_eq!(outcome.active.len(), 1);
        assert_eq!(outcome.active[0].constraint_kind, "max_fan_in");
        assert!(outcome.active[0].detail.contains("fan-in is 15"));
    }

    #[test]
    fn root_manifest_rename_change_rechecks_members() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, imported_path TEXT, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, language TEXT);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_external"
crates = ["arrow-core"]
name = "no-arrow"
"#,
        )
        .unwrap();

        // Current root manifest — no workspace renames
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"server\"]\n",
        )
        .unwrap();

        // Member manifest uses "innocent" with workspace = true
        let member_dir = dir.path().join("server");
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(
            member_dir.join("Cargo.toml"),
            "[package]\nname = \"server\"\n\n[dependencies]\ninnocent = { workspace = true }\n",
        )
        .unwrap();

        // Proposed root edit introduces workspace rename: innocent → arrow-core
        let proposed_root = r#"
[workspace]
members = ["server"]

[workspace.dependencies]
innocent = { package = "arrow-core", version = "1" }
"#;
        let outcome = check_proposed_manifest(&conn, dir.path(), "Cargo.toml", proposed_root);
        assert!(
            !outcome.active.is_empty(),
            "should flag member's workspace=true alias resolving to constrained package"
        );
        assert!(
            outcome
                .active
                .iter()
                .any(|f| f.to_path == "crate:arrow-core")
        );
    }

    #[test]
    fn root_rename_ignores_preexisting_member_violations() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, imported_path TEXT, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, language TEXT);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_external"
crates = ["arrow-core", "evil-crate"]
name = "no-bad-crates"
"#,
        )
        .unwrap();

        // Current root has no renames; member already has a direct forbidden dep
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"server\"]\n",
        )
        .unwrap();
        let member_dir = dir.path().join("server");
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(
            member_dir.join("Cargo.toml"),
            "[package]\nname = \"server\"\n\n[dependencies]\nevil-crate = \"1\"\ninnocent = { workspace = true }\n",
        )
        .unwrap();

        // Proposed root adds a rename for "innocent" → arrow-core, but should
        // NOT surface the pre-existing evil-crate violation
        let proposed_root = r#"
[workspace]
members = ["server"]

[workspace.dependencies]
innocent = { package = "arrow-core", version = "1" }
"#;
        let outcome = check_proposed_manifest(&conn, dir.path(), "Cargo.toml", proposed_root);
        let member_findings: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.from_path == "server/Cargo.toml")
            .collect();
        assert!(
            member_findings
                .iter()
                .any(|f| f.to_path == "crate:arrow-core"),
            "should flag the newly introduced arrow-core via rename"
        );
        assert!(
            !member_findings
                .iter()
                .any(|f| f.to_path == "crate:evil-crate"),
            "should NOT surface pre-existing evil-crate violation"
        );
    }

    #[test]
    fn root_rename_skips_non_workspace_members() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE imports (file_id INTEGER, imported_path TEXT, resolved_file_id INTEGER);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, language TEXT);
             CREATE TABLE components (id TEXT, name TEXT, prior_paths TEXT, dissolved_at TEXT);
             CREATE TABLE constraint_waivers (id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, file_path TEXT, symbol_qualified_name TEXT, rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', created_at TEXT DEFAULT '', updated_at TEXT DEFAULT '');",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(
            rules_dir.join("rules.toml"),
            r#"
[[constraint]]
kind = "forbidden_external"
crates = ["arrow-core"]
name = "no-arrow"
"#,
        )
        .unwrap();

        // Root declares only "server" as member, but a vendored crate also exists
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"server\"]\n",
        )
        .unwrap();
        let member_dir = dir.path().join("server");
        std::fs::create_dir_all(&member_dir).unwrap();
        std::fs::write(
            member_dir.join("Cargo.toml"),
            "[package]\nname = \"server\"\n\n[dependencies]\n",
        )
        .unwrap();

        // Vendored crate NOT in workspace.members, uses the alias
        let vendored_dir = dir.path().join("vendored");
        std::fs::create_dir_all(&vendored_dir).unwrap();
        std::fs::write(
            vendored_dir.join("Cargo.toml"),
            "[package]\nname = \"vendored\"\n\n[dependencies]\ninnocent = { workspace = true }\n",
        )
        .unwrap();

        let proposed_root = r#"
[workspace]
members = ["server"]

[workspace.dependencies]
innocent = { package = "arrow-core", version = "1" }
"#;
        let outcome = check_proposed_manifest(&conn, dir.path(), "Cargo.toml", proposed_root);
        let vendored_findings: Vec<_> = outcome
            .active
            .iter()
            .filter(|f| f.from_path == "vendored/Cargo.toml")
            .collect();
        assert!(
            vendored_findings.is_empty(),
            "should not check non-member vendored crate; got: {:?}",
            vendored_findings
                .iter()
                .map(|f| &f.to_path)
                .collect::<Vec<_>>()
        );
    }

    fn setup_signature_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE symbols (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 file_id INTEGER NOT NULL,
                 qualified_name TEXT NOT NULL,
                 short_name TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 signature TEXT,
                 signature_hash TEXT,
                 visibility TEXT,
                 start_line INTEGER NOT NULL DEFAULT 0,
                 start_col INTEGER NOT NULL DEFAULT 0,
                 end_line INTEGER NOT NULL DEFAULT 0,
                 end_col INTEGER NOT NULL DEFAULT 0,
                 parent_symbol_id INTEGER
             );
             INSERT INTO files VALUES (1, 'src/lib.rs');",
        )
        .unwrap();
        conn
    }

    fn insert_symbol(
        conn: &Connection,
        file_id: i64,
        name: &str,
        sig_hash: Option<&str>,
        vis: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO symbols (file_id, qualified_name, short_name, kind, signature_hash, visibility)
             VALUES (?1, ?2, ?2, 'function', ?3, ?4)",
            params![file_id, name, sig_hash, vis],
        )
        .unwrap();
    }

    #[test]
    fn signature_preserving_body_local_edit() {
        let conn = setup_signature_db();
        insert_symbol(&conn, 1, "do_thing", Some("abc123"), Some("pub"));
        let code = "pub fn do_thing(x: i32) -> bool {\n    x > 0\n}\n";
        let parsed = parse_proposed("src/lib.rs", code).unwrap();
        let proposed_flat = crate::parser::flatten_symbols(&parsed.symbols);
        assert_eq!(proposed_flat.len(), 1);
        let sym = proposed_flat[0];
        assert_eq!(sym.qualified_name, "do_thing");
        conn.execute(
            "UPDATE symbols SET signature_hash = ?1, visibility = ?2 WHERE qualified_name = 'do_thing'",
            params![sym.signature_hash, sym.visibility],
        ).unwrap();
        assert!(is_signature_preserving(&conn, 1, &parsed));
    }

    #[test]
    fn signature_change_not_preserving() {
        let conn = setup_signature_db();
        insert_symbol(&conn, 1, "do_thing", Some("old_hash"), Some("pub"));
        let code = "pub fn do_thing(x: i32, y: i32) -> bool {\n    x > y\n}\n";
        let parsed = parse_proposed("src/lib.rs", code).unwrap();
        assert!(!is_signature_preserving(&conn, 1, &parsed));
    }

    #[test]
    fn visibility_change_not_preserving() {
        let conn = setup_signature_db();
        let code = "pub(crate) fn do_thing(x: i32) -> bool {\n    x > 0\n}\n";
        let parsed = parse_proposed("src/lib.rs", code).unwrap();
        let sym = &crate::parser::flatten_symbols(&parsed.symbols)[0];
        // Same signature_hash but different visibility
        insert_symbol(
            &conn,
            1,
            "do_thing",
            sym.signature_hash.as_deref(),
            Some("pub"),
        );
        assert!(!is_signature_preserving(&conn, 1, &parsed));
    }

    #[test]
    fn symbol_deleted_not_preserving() {
        let conn = setup_signature_db();
        insert_symbol(&conn, 1, "do_thing", Some("abc"), Some("pub"));
        insert_symbol(&conn, 1, "helper", Some("def"), None);
        let code = "pub fn do_thing(x: i32) -> bool {\n    x > 0\n}\n";
        let parsed = parse_proposed("src/lib.rs", code).unwrap();
        let sym = &crate::parser::flatten_symbols(&parsed.symbols)[0];
        conn.execute(
            "UPDATE symbols SET signature_hash = ?1, visibility = ?2 WHERE qualified_name = 'do_thing'",
            params![sym.signature_hash, sym.visibility],
        ).unwrap();
        assert!(!is_signature_preserving(&conn, 1, &parsed));
    }

    #[test]
    fn new_symbol_added_is_preserving() {
        let conn = setup_signature_db();
        let code = "pub fn do_thing(x: i32) -> bool {\n    x > 0\n}\nfn helper() {}\n";
        let parsed = parse_proposed("src/lib.rs", code).unwrap();
        let sym = crate::parser::flatten_symbols(&parsed.symbols)
            .into_iter()
            .find(|s| s.qualified_name == "do_thing")
            .unwrap();
        insert_symbol(
            &conn,
            1,
            "do_thing",
            sym.signature_hash.as_deref(),
            sym.visibility.as_deref(),
        );
        assert!(is_signature_preserving(&conn, 1, &parsed));
    }

    #[test]
    fn no_indexed_symbols_is_preserving() {
        let conn = setup_signature_db();
        let code = "fn new_thing() {}\n";
        let parsed = parse_proposed("src/lib.rs", code).unwrap();
        assert!(is_signature_preserving(&conn, 1, &parsed));
    }

    #[test]
    fn duplicate_kind_deleted_not_preserving() {
        let conn = setup_signature_db();
        // Index a struct and an impl with the same qualified_name
        conn.execute(
            "INSERT INTO symbols (file_id, qualified_name, short_name, kind, signature_hash, visibility)
             VALUES (1, 'Foo', 'Foo', 'struct', NULL, 'pub')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO symbols (file_id, qualified_name, short_name, kind, signature_hash, visibility)
             VALUES (1, 'Foo', 'Foo', 'impl', NULL, NULL)",
            [],
        ).unwrap();
        // Proposed content only has the impl, struct deleted
        let code = "impl Foo {\n    fn bar() {}\n}\n";
        let parsed = parse_proposed("src/lib.rs", code).unwrap();
        assert!(!is_signature_preserving(&conn, 1, &parsed));
    }

    // -----------------------------------------------------------------------
    // Pattern guard tests
    // -----------------------------------------------------------------------

    fn setup_pattern_db(
        rules_toml: &str,
        files: &[(&str, &str)],
    ) -> (Connection, tempfile::TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE constraint_waivers (\
                id INTEGER PRIMARY KEY, constraint_id TEXT, constraint_name TEXT, \
                file_path TEXT, symbol_qualified_name TEXT, \
                rationale TEXT DEFAULT '', waived_by TEXT DEFAULT '', \
                created_at TEXT DEFAULT '', updated_at TEXT DEFAULT ''\
             );",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let rules_dir = dir.path().join(".sutra");
        std::fs::create_dir_all(&rules_dir).unwrap();
        std::fs::write(rules_dir.join("rules.toml"), rules_toml).unwrap();

        for (path, content) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, content).unwrap();
        }

        (conn, dir)
    }

    const CLONE_RULE: &str = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(call_expression function: (field_expression field: (field_identifier) @m (#eq? @m "clone"))) @match'
name = "no-clone"
severity = "blocking"
scope = "src/"
provenance = "coding_discipline"
"#;

    const ADVISORY_CLONE_RULE: &str = r#"
[[constraint]]
kind = "forbidden_pattern"
language = "rust"
query = '(call_expression function: (field_expression field: (field_identifier) @m (#eq? @m "clone"))) @match'
name = "no-clone"
severity = "advisory"
scope = "src/"
"#;

    #[test]
    fn pattern_introduced_denied() {
        let disk_content = "fn main() {\n    let x = 1;\n}\n";
        let proposed_content = "fn main() {\n    let x = vec![1].clone();\n}\n";
        let (conn, dir) = setup_pattern_db(CLONE_RULE, &[("src/lib.rs", disk_content)]);

        let outcome = check_proposed_patterns(&conn, dir.path(), "src/lib.rs", proposed_content);
        assert_eq!(outcome.active.len(), 1);
        assert_eq!(outcome.active[0].severity, Severity::Blocking);
        assert!(outcome.active[0].constraint_name.as_deref() == Some("no-clone"));
    }

    #[test]
    fn pattern_preexisting_allowed() {
        let disk_content = "fn main() {\n    let x = vec![1].clone();\n}\n";
        let proposed_content = "fn main() {\n    let x = vec![1].clone();\n    let y = 2;\n}\n";
        let (conn, dir) = setup_pattern_db(CLONE_RULE, &[("src/lib.rs", disk_content)]);

        let outcome = check_proposed_patterns(&conn, dir.path(), "src/lib.rs", proposed_content);
        assert!(
            outcome.active.is_empty(),
            "pre-existing match should be grandfathered"
        );
    }

    #[test]
    fn pattern_waiver_bypass_at_symbol_level() {
        let disk_content = "fn main() {\n    let x = 1;\n}\n";
        let proposed_content = "fn main() {\n    let x = vec![1].clone();\n}\n";
        let (conn, dir) = setup_pattern_db(CLONE_RULE, &[("src/lib.rs", disk_content)]);

        let rule_id = {
            let mut loaded = crate::rules::load_rules(dir.path()).unwrap();
            let (constraints, _) = loaded.all_constraints();
            constraints
                .iter()
                .find(|c| c.name.as_deref() == Some("no-clone"))
                .unwrap()
                .id
                .to_string()
        };

        conn.execute(
            "INSERT INTO constraint_waivers \
             (constraint_id, constraint_name, file_path, symbol_qualified_name, rationale, waived_by) \
             VALUES (?1, 'no-clone', 'src/lib.rs', 'main', 'API requires owned', 'test')",
            params![rule_id],
        )
        .unwrap();

        let outcome = check_proposed_patterns(&conn, dir.path(), "src/lib.rs", proposed_content);
        assert!(
            outcome.active.is_empty(),
            "waived symbol should be excluded from both counts"
        );
        assert_eq!(outcome.waived.len(), 1);
    }

    #[test]
    fn pattern_advisory_passthrough() {
        let disk_content = "fn main() {\n    let x = 1;\n}\n";
        let proposed_content = "fn main() {\n    let x = vec![1].clone();\n}\n";
        let (conn, dir) = setup_pattern_db(ADVISORY_CLONE_RULE, &[("src/lib.rs", disk_content)]);

        let outcome = check_proposed_patterns(&conn, dir.path(), "src/lib.rs", proposed_content);
        assert_eq!(outcome.active.len(), 1);
        assert_eq!(outcome.active[0].severity, Severity::Advisory);
    }

    #[test]
    fn pattern_write_tool_new_file() {
        let proposed_content = "fn process() {\n    let data = input.clone();\n}\n";
        let (conn, dir) = setup_pattern_db(CLONE_RULE, &[]);

        let outcome =
            check_proposed_patterns(&conn, dir.path(), "src/new_file.rs", proposed_content);
        assert_eq!(
            outcome.active.len(),
            1,
            "Write to new file: all matches are introduced"
        );
    }

    #[test]
    fn pattern_introduced_only_reports_delta() {
        let disk_content = "fn main() {\n\
            \x20   let a = vec![1].clone();\n\
            \x20   let b = vec![2].clone();\n\
            }\n";
        let proposed_content = "fn main() {\n\
            \x20   let a = vec![1].clone();\n\
            \x20   let b = vec![2].clone();\n\
            \x20   let c = vec![3].clone();\n\
            }\n";
        let (conn, dir) = setup_pattern_db(CLONE_RULE, &[("src/lib.rs", disk_content)]);

        let outcome = check_proposed_patterns(&conn, dir.path(), "src/lib.rs", proposed_content);
        assert_eq!(
            outcome.active.len(),
            1,
            "only the 1 introduced match should be active, not all 3"
        );
    }
}
