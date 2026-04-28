use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

use rusqlite::{Connection, OpenFlags, params};

use sutra::guard::{self, FileFacts, GuardConfig, HookInput};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sutra-guard: {e}");
            ExitCode::SUCCESS // fail-open
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
    let ack_fresh = guard::ack_is_fresh(&project_root, &rel_path, cfg.ack_ttl_secs);
    let decision = guard::evaluate(&facts, &cfg, ack_fresh);

    if let Some(json) = guard::render_stdout(&decision, hook.hook_event_name.as_deref()) {
        println!("{json}");
    }

    Ok(())
}

fn resolve_project_root(hook_cwd: Option<&str>, file_path: Option<&std::path::Path>) -> Option<PathBuf> {
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
