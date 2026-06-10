use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use rusqlite::{Connection, OpenFlags, params};

use sutra::guard::{self, FileFacts, GuardConfig, HookInput};
use sutra::rules::Severity;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--check-constraints") {
        let staged = args.iter().any(|a| a == "--staged");
        match run_check_constraints(staged) {
            Ok(has_blocking) => {
                if has_blocking {
                    ExitCode::from(1)
                } else {
                    ExitCode::SUCCESS
                }
            }
            Err(e) => {
                eprintln!("sutra-guard: {e}");
                ExitCode::SUCCESS // fail-open
            }
        }
    } else {
        match run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("sutra-guard: {e}");
                ExitCode::SUCCESS // fail-open
            }
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    if GuardConfig::is_disabled() {
        return Ok(());
    }

    let cfg = GuardConfig::from_env();

    let mut stdin_buf = String::new();
    std::io::stdin().read_to_string(&mut stdin_buf)?;
    let hook: HookInput = serde_json::from_str(&stdin_buf)?;

    let tool_lower = hook.tool_name.to_lowercase();

    // Routing guard: deny Glob/Grep when sutra index exists.
    if matches!(tool_lower.as_str(), "glob" | "grep" | "grep_search") {
        let project_root = resolve_project_root(hook.cwd.as_deref(), None);
        if let Some(root) = project_root {
            let ws_id = guard::workspace_id_from_path(&root);
            let db_dir = guard::sutra_db_dir();
            let db_path = db_dir.join(&ws_id).join("index.db");
            if db_path.is_file() {
                let reason = if tool_lower == "grep" {
                    "STOP: sutra MCP is available. Use `sutra_grep` or `sutra_find` \
                     instead of Grep for indexed symbol search."
                } else {
                    "STOP: sutra MCP is available. Use `sutra_map` instead of Glob \
                     for project structure, or `sutra_find` to locate symbols."
                };
                if let Some(json) = guard::render_stdout(
                    &guard::GuardDecision::Deny {
                        reason: reason.to_string(),
                    },
                    hook.hook_event_name.as_deref(),
                ) {
                    println!("{json}");
                }
            }
        }
        return Ok(());
    }

    // Modification guard: only for edit/write tools.
    if !matches!(
        tool_lower.as_str(),
        "edit" | "write" | "multiedit" | "replace" | "write_file"
    ) {
        return Ok(());
    }

    let file_path_raw = match hook.tool_input.file_path {
        Some(ref p) => p.clone(),
        None => return Ok(()),
    };
    let file_path = PathBuf::from(&file_path_raw);

    let project_root = match resolve_project_root(hook.cwd.as_deref(), Some(&file_path)) {
        Some(r) => r,
        None => return Ok(()),
    };

    let ws_id = guard::workspace_id_from_path(&project_root);
    let db_dir = guard::sutra_db_dir();
    let db_path = db_dir.join(&ws_id).join("index.db");
    if !db_path.is_file() {
        return Ok(());
    }

    let rel_path = match guard::relativize_file_path(&project_root, &file_path) {
        Some(p) => p.replace('\\', "/"),
        None => return Ok(()),
    };

    let conn = Connection::open_with_flags(
        &db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_millis(500))?;

    let file_row: Option<(i64, f64, i64)> = conn
        .query_row(
            "SELECT id, COALESCE(pagerank, 0.0), blast_radius FROM files WHERE path = ?1",
            params![rel_path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok();

    let Some((file_id, pagerank, blast_radius)) = file_row else {
        return Ok(());
    };

    let hot_symbols: Vec<(String, f64)> = conn
        .prepare(
            "SELECT short_name, COALESCE(pagerank, 0.0) FROM symbols \
             WHERE file_id = ?1 AND pagerank IS NOT NULL \
             ORDER BY pagerank DESC LIMIT 3",
        )
        .and_then(|mut stmt| {
            stmt.query_map(params![file_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .unwrap_or_default();

    let facts = FileFacts {
        rel_path: rel_path.clone(),
        pagerank,
        blast_radius,
        hot_symbols,
    };

    // Constraint check: try proposed-content analysis first, fall back to indexed edges
    let constraint_findings = if let Some(proposed) =
        guard::build_proposed_content(&hook.tool_input, &project_root, &rel_path)
    {
        if let Some(proposed_outgoing) = guard::extract_proposed_outgoing_edges(
            &conn,
            &project_root,
            &rel_path,
            file_id,
            &proposed,
        ) {
            guard::check_proposed_file_constraints(
                &conn,
                &project_root,
                file_id,
                &proposed_outgoing,
            )
        } else {
            guard::check_file_constraints(&conn, &project_root, file_id)
        }
    } else {
        guard::check_file_constraints(&conn, &project_root, file_id)
    };

    let blocking: Vec<_> = constraint_findings
        .active
        .iter()
        .filter(|f| f.severity == Severity::Blocking)
        .collect();
    let advisory: Vec<_> = constraint_findings
        .active
        .iter()
        .filter(|f| f.severity != Severity::Blocking)
        .collect();

    for f in &advisory {
        eprintln!(
            "sutra-guard: [{:?}] {} — {}",
            f.severity, f.constraint_id, f.detail
        );
    }

    if !blocking.is_empty() {
        let reason = guard::format_constraint_deny(&blocking);
        if let Some(json) = guard::render_stdout(
            &guard::GuardDecision::Deny { reason },
            hook.hook_event_name.as_deref(),
        ) {
            println!("{json}");
        }
        return Ok(());
    }

    if guard::is_additive_edit(&hook.tool_input) {
        return Ok(());
    }

    let ack_fresh = guard::ack_is_fresh(&project_root, &rel_path, cfg.ack_ttl_secs);
    let decision = guard::evaluate(&facts, &cfg, ack_fresh);

    if let Some(json) = guard::render_stdout(&decision, hook.hook_event_name.as_deref()) {
        println!("{json}");
    }

    Ok(())
}

fn run_check_constraints(staged: bool) -> Result<bool, Box<dyn std::error::Error>> {
    let project_root = std::env::current_dir()
        .ok()
        .and_then(|d| guard::find_project_root(&d))
        .ok_or("cannot find project root (no .git found)")?;

    let ws_id = guard::workspace_id_from_path(&project_root);
    let db_dir = guard::sutra_db_dir();
    let db = sutra::db::Db::open_unchecked(&ws_id, &db_dir)?;

    let (changed_paths, base_revision) = if staged {
        (
            sutra::git::git_diff_staged(&project_root)?,
            "HEAD".to_string(),
        )
    } else {
        let default_branch = sutra::git::detect_default_branch(&project_root)?;
        let base = sutra::git::git_merge_base(&project_root, &default_branch)?;
        let paths = sutra::git::git_diff_files(&project_root, &base, "HEAD")?;
        (paths, base)
    };

    if changed_paths.is_empty() {
        println!("{{}}");
        return Ok(false);
    }

    let registry = sutra::parser::adapter::default_registry();
    let findings = sutra::tools::review::build_findings(
        &db,
        &project_root,
        &changed_paths,
        &base_revision,
        None,
        &registry,
    )?;

    let mut blocking = Vec::new();
    let mut advisory = Vec::new();
    let mut informational = Vec::new();

    for v in &findings.constraint_violations {
        let entry = serde_json::json!({
            "constraint_id": v.constraint_id,
            "constraint_name": v.constraint_name,
            "kind": v.constraint_kind,
            "severity": v.severity.as_str(),
            "from": v.from_path,
            "to": v.to_path,
            "detail": v.detail,
        });
        match v.severity {
            Severity::Blocking => blocking.push(entry),
            Severity::Advisory => advisory.push(entry),
            _ => informational.push(entry),
        }
    }

    let waived: Vec<_> = findings
        .waived_constraint_violations
        .iter()
        .map(|v| {
            serde_json::json!({
                "constraint_id": v.finding.constraint_id,
                "constraint_name": v.finding.constraint_name,
                "kind": v.finding.constraint_kind,
                "severity": v.finding.severity.as_str(),
                "from": v.finding.from_path,
                "to": v.finding.to_path,
                "detail": v.finding.detail,
                "rationale": v.rationale,
                "waived_by": v.waived_by,
            })
        })
        .collect();

    let has_blocking = !blocking.is_empty();

    let output = serde_json::json!({
        "blocking": blocking,
        "advisory": advisory,
        "informational": informational,
        "waived": waived,
        "exit_code": if has_blocking { 1 } else { 0 },
    });

    println!("{}", serde_json::to_string_pretty(&output)?);

    if has_blocking {
        eprintln!(
            "sutra-guard: {} blocking constraint violation(s) found",
            blocking.len()
        );
    }

    Ok(has_blocking)
}

fn resolve_project_root(
    hook_cwd: Option<&str>,
    file_path: Option<&std::path::Path>,
) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("CLAUDE_PROJECT_DIR") {
        let candidate = PathBuf::from(&explicit);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    if let Some(cwd) = hook_cwd
        && let Some(root) = guard::find_project_root(std::path::Path::new(cwd))
    {
        return Some(root);
    }
    if let Some(fp) = file_path
        && let Some(parent) = fp.parent()
        && let Some(root) = guard::find_project_root(parent)
    {
        return Some(root);
    }
    std::env::current_dir()
        .ok()
        .and_then(|d| guard::find_project_root(&d))
}
