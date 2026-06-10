use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde::Deserialize;

use crate::constraints::check::{self, CheckOutcome, EvalScope, FactsSource};

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

pub use check::ConstraintFinding;

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

pub fn extract_proposed_outgoing_edges(
    conn: &Connection,
    project_root: &Path,
    rel_path: &str,
    file_id: i64,
    proposed_content: &str,
) -> Option<Vec<(i64, i64)>> {
    let language = language_from_path(rel_path)?;

    let result = crate::parser::parse_file(proposed_content, language, rel_path).ok()?;
    if !result.parsed_ok {
        return None;
    }

    if language != "rust" {
        return None;
    }

    let crate_name = crate::rust_imports::read_crate_name(project_root);

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
        let segments = match crate::rust_imports::normalize_to_crate_segments(
            &import.raw_path,
            rel_path,
            crate_name.as_deref(),
        ) {
            Some(s) if !s.is_empty() => s,
            _ => continue,
        };
        if let Some(target_id) = crate::rust_imports::resolve_segments(&segments, &path_ref_map) {
            if target_id != file_id {
                edges.push((file_id, target_id));
            }
        }
    }
    Some(edges)
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
) -> CheckOutcome {
    let mut edges: Vec<(i64, i64)> = proposed_outgoing.to_vec();
    edges.extend(get_incoming_edges(conn, file_id));
    check::evaluate(
        &FactsSource::RawConn(conn),
        project_root,
        EvalScope::Edges(&edges),
    )
    .unwrap_or_default()
}

pub fn check_file_constraints(
    conn: &Connection,
    project_root: &Path,
    file_id: i64,
) -> CheckOutcome {
    check::evaluate(
        &FactsSource::RawConn(conn),
        project_root,
        EvalScope::SingleFile(file_id),
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
    use crate::constraints::check::FindingDelta;
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
        use crate::constraints::check::WaivedFinding;
        let outcome = CheckOutcome {
            waived: vec![WaivedFinding {
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
        use crate::constraints::check::WaivedFinding;
        let outcome = CheckOutcome {
            active: vec![
                make_finding(Severity::Blocking),
                make_finding(Severity::Advisory),
                make_finding(Severity::Informational),
            ],
            waived: vec![WaivedFinding {
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
             CREATE TABLE constraint_waivers (constraint_id TEXT, file_path TEXT);",
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
             CREATE TABLE constraint_waivers (constraint_id TEXT, file_path TEXT);
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
             CREATE TABLE constraint_waivers (constraint_id TEXT, file_path TEXT);
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
            "INSERT INTO constraint_waivers VALUES (?1, 'src/tools/review.rs')",
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
             CREATE TABLE constraint_waivers (constraint_id TEXT, file_path TEXT);
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
             CREATE TABLE constraint_waivers (constraint_id TEXT, file_path TEXT);
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
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing);
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
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing);
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
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing);
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
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing);
        assert_eq!(outcome.active.len(), 1);
        let constraint_id = outcome.active[0].constraint_id.clone();

        // Waiver on the TARGET file (daemon.rs), not the source
        conn.execute(
            "INSERT INTO constraint_waivers VALUES (?1, 'src/daemon.rs')",
            params![constraint_id],
        )
        .unwrap();

        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing);
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
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing);
        assert_eq!(outcome.active.len(), 1);
        let constraint_id = outcome.active[0].constraint_id.clone();

        // Waiver on the SOURCE file (tools/review.rs)
        conn.execute(
            "INSERT INTO constraint_waivers VALUES (?1, 'src/tools/review.rs')",
            params![constraint_id],
        )
        .unwrap();

        let outcome = check_proposed_file_constraints(&conn, dir.path(), 1, &proposed_outgoing);
        assert!(outcome.active.is_empty());
        assert_eq!(outcome.waived.len(), 1);
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
        let outcome = check_proposed_file_constraints(&conn, dir.path(), 2, &proposed_outgoing);
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
}
