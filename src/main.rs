use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use parking_lot::{Mutex, RwLock};
use sutra::config::Config;
use sutra::db::Db;
use sutra::workspace::{self, WorkspaceEntry};

#[derive(Parser)]
#[command(name = "sutra", about = "Code intelligence for manas", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the sutra server
    Serve {
        /// Use stdio transport instead of HTTP
        #[arg(long)]
        stdio: bool,
    },
    /// Parse a workspace
    Parse {
        /// Workspace id
        workspace: String,
    },
    /// Manage workspaces
    #[command(subcommand)]
    Workspaces(WorkspacesCmd),
    /// Manage the modification guard hooks
    #[command(subcommand)]
    Guard(GuardCmd),
    /// Query the file map (JSON output)
    Map {
        /// Workspace id
        workspace: String,
        /// Only include files under this path prefix
        #[arg(long)]
        path_prefix: Option<String>,
        /// Max results
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Search symbols by pattern (JSON output)
    Grep {
        /// Workspace id
        workspace: String,
        /// Search pattern
        pattern: String,
        /// Filter by symbol kind
        #[arg(long)]
        kind: Option<String>,
        /// Max results
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Find a symbol by name (JSON output)
    Find {
        /// Workspace id
        workspace: String,
        /// Symbol name
        name: String,
        /// Filter by symbol kind
        #[arg(long)]
        kind: Option<String>,
        /// Max results
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Read a symbol's source code (JSON output)
    Read {
        /// Workspace id
        workspace: String,
        /// Symbol name
        symbol: String,
        /// Context lines around the symbol
        #[arg(long)]
        context_lines: Option<usize>,
    },
    /// Show file outline — symbol table of contents (JSON output)
    Outline {
        /// Workspace id
        workspace: String,
        /// File path (relative to workspace root)
        path: String,
    },
    /// Blast radius analysis for a symbol (JSON output)
    Impact {
        /// Workspace id
        workspace: String,
        /// Symbol name
        symbol: String,
    },
    /// Manage constraint ratchets (CLI-only, not exposed via MCP)
    #[command(subcommand)]
    Ratchet(RatchetCmd),
    /// Check server and database health
    Health,
    /// Install systemd user service for sutra
    InstallServices {
        /// Enable and start the service after installing
        #[arg(long)]
        enable: bool,
    },
}

#[derive(Subcommand)]
enum GuardCmd {
    /// Install sutra hooks into Claude Code settings (removes qartez hooks)
    Install,
    /// Remove sutra hooks from Claude Code settings
    Uninstall,
}

#[derive(Subcommand)]
enum WorkspacesCmd {
    /// Register a new workspace
    Add {
        /// Workspace identifier
        id: String,
        /// Root directory path
        root: String,
        /// Languages to index
        languages: Vec<String>,
    },
    /// List registered workspaces
    List,
    /// Remove a workspace
    Remove {
        /// Workspace identifier
        id: String,
    },
}

#[derive(Subcommand)]
enum RatchetCmd {
    /// Release a ratcheted constraint (allows its removal or weakening)
    Release {
        /// Constraint ID (8-char hex from rules.toml)
        constraint_id: String,
        /// Why this constraint is being released
        #[arg(long)]
        rationale: String,
        /// Who is releasing (defaults to git config user.name)
        #[arg(long)]
        by: Option<String>,
    },
    /// List all ratcheted constraints (active and released)
    List,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    let config = Arc::new(Config::from_env()?);

    let use_stderr = matches!(cli.command, Commands::Serve { stdio: true });
    if use_stderr {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.log_level)),
            )
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.log_level)),
            )
            .init();
    }

    match cli.command {
        Commands::Serve { stdio } => {
            if stdio {
                cmd_serve_stdio(config).await?;
            } else {
                cmd_serve_http(config).await?;
            }
        }
        Commands::Parse { workspace: ws_id } => {
            let ws_config = load_validated_workspaces(&config)?;
            let ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open_for_workspace(ws, &config.db_dir)?;
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let registry = sutra::parser::adapter::default_registry();
            let snapshot = sutra::pipeline::parse_workspace(ws, &db, &config, &cancel, &registry)?;
            let resolvable = snapshot.resolved_count + snapshot.unresolved_count;
            let pct = if resolvable > 0 {
                snapshot.resolved_count * 100 / resolvable
            } else {
                0
            };
            println!(
                "Parsed {}/{} files changed, {} symbols, {} refs ({} resolved of {} resolvable, {}%; {} skipped) in {}ms",
                snapshot.files_parsed,
                snapshot.files_walked,
                snapshot.symbols_extracted,
                snapshot.refs_extracted,
                snapshot.resolved_count,
                resolvable,
                pct,
                snapshot.skipped_count,
                snapshot.duration_ms
            );
        }
        Commands::Map {
            workspace: ws_id,
            path_prefix,
            limit,
        } => {
            let ws_config = load_validated_workspaces(&config)?;
            let ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open_for_workspace(ws, &config.db_dir)?;
            let result = sutra::tools::map::handle(&db, path_prefix.as_deref(), limit, false)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Grep {
            workspace: ws_id,
            pattern,
            kind,
            limit,
        } => {
            let ws_config = load_validated_workspaces(&config)?;
            let ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open_for_workspace(ws, &config.db_dir)?;
            let result = sutra::tools::grep::handle(&db, &pattern, kind.as_deref(), limit, false)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Find {
            workspace: ws_id,
            name,
            kind,
            limit,
        } => {
            let ws_config = load_validated_workspaces(&config)?;
            let ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open_for_workspace(ws, &config.db_dir)?;
            let result = sutra::tools::find::handle(&db, &name, kind.as_deref(), limit, false)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Read {
            workspace: ws_id,
            symbol,
            context_lines,
        } => {
            let ws_config = load_validated_workspaces(&config)?;
            let ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open_for_workspace(ws, &config.db_dir)?;
            let result = sutra::tools::read::handle(
                &db,
                &ws.root,
                &symbol,
                context_lines,
                None,
                false,
                false,
                true,
                None,
            )?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Outline {
            workspace: ws_id,
            path,
        } => {
            let ws_config = load_validated_workspaces(&config)?;
            let ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open_for_workspace(ws, &config.db_dir)?;
            let result = sutra::tools::outline::handle(&db, &path, false)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Impact {
            workspace: ws_id,
            symbol,
        } => {
            let ws_config = load_validated_workspaces(&config)?;
            let ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open_for_workspace(ws, &config.db_dir)?;
            let ws_root = std::path::Path::new(&ws.root);
            let result = sutra::tools::impact::handle(&db, &symbol, false, None, ws_root)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Workspaces(cmd) => match cmd {
            WorkspacesCmd::Add {
                id,
                root,
                languages,
            } => {
                let entry = WorkspaceEntry {
                    id: id.clone(),
                    root: PathBuf::from(root),
                    languages,
                };
                workspace::validate_db_dir_for_workspace(&config.db_dir, &entry)?;
                workspace::add_workspace(&config.workspaces_path, entry)?;
                println!("Workspace '{id}' added.");
            }
            WorkspacesCmd::List => {
                let entries = workspace::list_workspaces(&config.workspaces_path)?;
                if entries.is_empty() {
                    println!("No workspaces registered.");
                } else {
                    for e in &entries {
                        println!(
                            "{}\t{}\t[{}]",
                            e.id,
                            e.root.display(),
                            e.languages.join(", ")
                        );
                    }
                }
            }
            WorkspacesCmd::Remove { id } => {
                workspace::remove_workspace(&config.workspaces_path, &id)?;
                println!("Workspace '{id}' removed.");
            }
        },
        Commands::Guard(cmd) => match cmd {
            GuardCmd::Install => {
                sutra::guard::install()?;
            }
            GuardCmd::Uninstall => {
                sutra::guard::uninstall()?;
            }
        },
        Commands::Ratchet(cmd) => {
            cmd_ratchet(&config, cmd)?;
        }
        Commands::Health => {
            cmd_health(&config).await?;
        }
        Commands::InstallServices { enable } => {
            cmd_install_services(enable)?;
        }
    }

    Ok(())
}

type DbCache = Arc<Mutex<HashMap<String, Arc<Db>>>>;
type WsConfig = Arc<RwLock<workspace::WorkspacesConfig>>;

fn load_validated_workspaces(config: &Config) -> sutra::error::Result<workspace::WorkspacesConfig> {
    let ws_config = workspace::load_workspaces(&config.workspaces_path)?;
    workspace::validate_db_dir_outside_workspaces(&config.db_dir, &ws_config)?;
    Ok(ws_config)
}

fn load_workspaces_and_cache(
    config: &Config,
) -> Result<(WsConfig, DbCache), Box<dyn std::error::Error>> {
    let ws_config = load_validated_workspaces(config)?;
    let ws_config = Arc::new(RwLock::new(ws_config));
    let db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>> = Arc::new(Mutex::new(HashMap::new()));
    Ok((ws_config, db_cache))
}

async fn cmd_serve_stdio(config: Arc<Config>) -> Result<(), Box<dyn std::error::Error>> {
    use rmcp::ServiceExt;
    use rmcp::transport::stdio;
    use sutra::mcp::SutraServer;

    let (ws_config, db_cache) = load_workspaces_and_cache(&config)?;
    let parse_coord = sutra::pipeline::ParseCoordinator::new();
    let lessons_db = Arc::new(
        sutra::lessons::LessonsDb::open(&config.db_dir).expect("failed to open lessons.db"),
    );
    let server = SutraServer::new(
        config.clone(),
        ws_config.clone(),
        db_cache.clone(),
        parse_coord.clone(),
        lessons_db,
    );

    // Background reparse: if CWD is inside a registered workspace and the
    // index is stale, kick off a parse without blocking the MCP handshake.
    maybe_reparse_cwd(&config, &ws_config, &db_cache, &parse_coord);

    let (stdin, stdout) = stdio();
    let service = server.serve((stdin, stdout)).await?;

    tokio::select! {
        res = service.waiting() => { res?; }
        _ = shutdown_signal() => { tracing::info!("shutdown signal received"); }
    }
    Ok(())
}

fn maybe_reparse_cwd(
    config: &Arc<Config>,
    ws_config: &WsConfig,
    db_cache: &DbCache,
    parse_coord: &sutra::pipeline::ParseCoordinator,
) {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(_) => return,
    };

    let ws_cfg = ws_config.read();
    let entry = ws_cfg.workspace.iter().find(|w| cwd.starts_with(&w.root));
    let entry = match entry {
        Some(e) => e.clone(),
        None => return,
    };
    drop(ws_cfg);

    let db = match sutra::tools::get_or_open_db(db_cache, &entry, &config.db_dir) {
        Ok(db) => db,
        Err(_) => return,
    };

    let (_, is_stale) =
        sutra::freshness::is_workspace_stale(&db, &entry.root, config.stale_threshold_sec);
    if !is_stale {
        return;
    }

    let lock = parse_coord.lock_for(&entry.id);
    let Ok(guard) = lock.clone().try_lock_owned() else {
        return;
    };
    let mark = parse_coord.mark_parsing(&entry.id);

    let config = Arc::clone(config);
    let ws_id = entry.id.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let _mark = mark;
        let result = tokio::task::spawn_blocking(move || {
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let registry = sutra::parser::adapter::default_registry();
            sutra::pipeline::parse_workspace(&entry, &db, &config, &cancel, &registry)
        })
        .await;
        match result {
            Ok(Ok(snap)) => {
                tracing::info!(
                    "stdio startup reparse for {ws_id}: {}/{} files, {} symbols in {}ms",
                    snap.files_parsed,
                    snap.files_walked,
                    snap.symbols_extracted,
                    snap.duration_ms,
                );
            }
            Ok(Err(e)) => tracing::error!("stdio startup reparse failed for {ws_id}: {e}"),
            Err(e) => tracing::error!("stdio startup reparse panicked for {ws_id}: {e}"),
        }
    });
}

async fn cmd_serve_http(config: Arc<Config>) -> Result<(), Box<dyn std::error::Error>> {
    use axum::routing::any_service;
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager,
        tower::{StreamableHttpServerConfig, StreamableHttpService},
    };
    use sutra::mcp::SutraServer;
    use sutra::pipeline::ParseCoordinator;
    use tokio::net::TcpListener;
    use tokio_util::sync::CancellationToken;

    let addr = config.listen_addr.clone();

    if tokio::net::TcpStream::connect(&addr).await.is_ok() {
        eprintln!("sutra already running on {addr}");
        std::process::exit(0);
    }

    let pid_path = config.db_dir.join("sutra.pid");
    std::fs::create_dir_all(&config.db_dir)?;
    std::fs::write(&pid_path, std::process::id().to_string())?;

    let (ws_config, db_cache) = load_workspaces_and_cache(&config)?;

    let parse_coord = ParseCoordinator::new();
    let dd_engines = Arc::new(parking_lot::Mutex::new(std::collections::HashMap::<
        String,
        Arc<sutra::constraints::DdEngine>,
    >::new()));
    let lessons_db = Arc::new(
        sutra::lessons::LessonsDb::open(&config.db_dir).expect("failed to open lessons.db"),
    );

    let cancel = CancellationToken::new();
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config.keep_alive = None;
    let session_manager = Arc::new(session_manager);
    let shttp_config =
        StreamableHttpServerConfig::default().with_cancellation_token(cancel.clone());

    let cfg_clone = config.clone();
    let ws_clone = ws_config.clone();
    let db_clone = db_cache.clone();
    let coord_clone = parse_coord.clone();
    let dd_clone = dd_engines.clone();
    let lessons_clone = lessons_db.clone();
    let mcp_service = StreamableHttpService::new(
        move || {
            Ok(SutraServer::new(
                cfg_clone.clone(),
                ws_clone.clone(),
                db_clone.clone(),
                coord_clone.clone(),
                lessons_clone.clone(),
            )
            .with_dd_engines(dd_clone.clone()))
        },
        session_manager,
        shttp_config,
    );

    let rest = sutra::rest::router(config.clone(), ws_config.clone(), db_cache.clone());

    #[allow(deprecated)]
    let app = rest.route("/mcp", any_service(mcp_service));

    let listener = TcpListener::bind(&addr).await?;
    tracing::info!("sutra listening on {addr}");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    let _ = std::fs::remove_file(&pid_path);
    tracing::info!("sutra shut down");
    Ok(())
}

async fn cmd_health(config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let addr = &config.listen_addr;
    let server_running = tokio::net::TcpStream::connect(addr).await.is_ok();

    let ws_config = load_validated_workspaces(config)?;

    println!(
        "server:     {}",
        if server_running {
            "running"
        } else {
            "not running"
        }
    );
    println!("listen:     {addr}");
    println!("workspaces: {}", ws_config.workspace.len());

    for ws in &ws_config.workspace {
        match Db::open_for_workspace(ws, &config.db_dir) {
            Ok(db) => {
                let files = db.all_files().unwrap_or_default();
                let last_parse = db.last_parse_time().unwrap_or(None);
                println!(
                    "  {} — {} files, last parse: {}",
                    ws.id,
                    files.len(),
                    last_parse.as_deref().unwrap_or("never")
                );
            }
            Err(e) => {
                println!("  {} — error: {e}", ws.id);
            }
        }
    }
    Ok(())
}

fn cmd_install_services(enable: bool) -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var("HOME").map_err(|_| "HOME not set")?;
    let sutra_bin = format!("{home}/.cargo/bin/sutra");

    let unit_content = format!(
        r#"[Unit]
Description=sutra code-intelligence server
After=default.target

[Service]
Type=simple
ExecStart={sutra_bin} serve
Restart=always
RestartSec=2
TimeoutStopSec=30
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
"#
    );

    let service_dir = format!("{home}/.config/systemd/user");
    std::fs::create_dir_all(&service_dir)?;

    let service_path = format!("{service_dir}/sutra.service");
    std::fs::write(&service_path, unit_content)?;
    println!("Wrote {service_path}");

    let status = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()?;
    if !status.success() {
        return Err("systemctl --user daemon-reload failed".into());
    }
    println!("Reloaded systemd user units");

    if enable {
        let status = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", "sutra.service"])
            .status()?;
        if !status.success() {
            return Err("systemctl --user enable --now sutra.service failed".into());
        }
        println!("Enabled and started sutra.service");
    }

    Ok(())
}

fn cmd_ratchet(config: &Config, cmd: RatchetCmd) -> Result<(), Box<dyn std::error::Error>> {
    let ws_config = load_validated_workspaces(config)?;
    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.to_string_lossy();
    let ws = workspace::resolve_workspace(&ws_config, &cwd_str)?;
    let db = Db::open_for_workspace(ws, &config.db_dir)?;

    match cmd {
        RatchetCmd::Release {
            constraint_id,
            rationale,
            by,
        } => {
            let row = db.get_constraint_ratchet(&constraint_id)?;
            match row {
                None => {
                    eprintln!("error: unknown constraint id '{constraint_id}'");
                    std::process::exit(1);
                }
                Some(r) if r.released_at.is_some() => {
                    eprintln!(
                        "error: constraint '{constraint_id}' already released by {} on {}",
                        r.released_by.as_deref().unwrap_or("unknown"),
                        r.released_at.as_deref().unwrap_or("unknown"),
                    );
                    std::process::exit(1);
                }
                Some(r) => {
                    let released_by = by.unwrap_or_else(|| {
                        std::process::Command::new("git")
                            .args(["config", "user.name"])
                            .output()
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "unknown".to_string())
                    });
                    db.release_constraint_ratchet(&constraint_id, &released_by, &rationale)?;
                    let label = r.name.as_deref().unwrap_or(constraint_id.as_str());
                    println!("Released ratchet [{label}] ({constraint_id})");
                    println!("  by: {released_by}");
                    println!("  rationale: {rationale}");
                }
            }
        }
        RatchetCmd::List => {
            let rows = db.get_all_constraint_ratchets()?;
            if rows.is_empty() {
                println!("No ratcheted constraints.");
                return Ok(());
            }
            for r in &rows {
                let label = r.name.as_deref().unwrap_or("");
                let status = if r.released_at.is_some() {
                    "released"
                } else {
                    "active"
                };
                println!(
                    "{id}  {label:<24} {sev:<12} {status:<10} registered {reg}",
                    id = r.constraint_id,
                    sev = r.severity_floor,
                    reg = r.registered_at,
                );
                if let Some(ref at) = r.released_at {
                    println!(
                        "        released {at} by {by}: {rationale}",
                        by = r.released_by.as_deref().unwrap_or("unknown"),
                        rationale = r.release_rationale.as_deref().unwrap_or(""),
                    );
                }
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("failed to install CTRL+C handler: {e}");
    }
}
