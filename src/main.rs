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
    /// Start the sutra server (default: HTTP daemon)
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
    /// Check daemon and database health
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
            let ws_config = workspace::load_workspaces(&config.workspaces_path)?;
            let ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open(&ws_id, &config.db_dir)?;
            let cancel = std::sync::atomic::AtomicBool::new(false);
            let snapshot = sutra::pipeline::parse_workspace(ws, &db, &config, &cancel)?;
            let resolvable = snapshot.refs_extracted - snapshot.skipped_count;
            let resolved = resolvable - snapshot.unresolved_count;
            let pct = if resolvable > 0 {
                resolved * 100 / resolvable
            } else {
                0
            };
            println!(
                "Parsed {}/{} files changed, {} symbols, {} refs ({} resolved of {} resolvable, {}%; {} skipped) in {}ms",
                snapshot.files_parsed,
                snapshot.files_walked,
                snapshot.symbols_extracted,
                snapshot.refs_extracted,
                resolved,
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
            let ws_config = workspace::load_workspaces(&config.workspaces_path)?;
            let _ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open(&ws_id, &config.db_dir)?;
            let result = sutra::tools::map::handle(&db, path_prefix.as_deref(), limit)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Grep {
            workspace: ws_id,
            pattern,
            kind,
            limit,
        } => {
            let ws_config = workspace::load_workspaces(&config.workspaces_path)?;
            let _ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open(&ws_id, &config.db_dir)?;
            let result = sutra::tools::grep::handle(&db, &pattern, kind.as_deref(), limit)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Find {
            workspace: ws_id,
            name,
            kind,
            limit,
        } => {
            let ws_config = workspace::load_workspaces(&config.workspaces_path)?;
            let _ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open(&ws_id, &config.db_dir)?;
            let result = sutra::tools::find::handle(&db, &name, kind.as_deref(), limit)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Read {
            workspace: ws_id,
            symbol,
            context_lines,
        } => {
            let ws_config = workspace::load_workspaces(&config.workspaces_path)?;
            let ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open(&ws_id, &config.db_dir)?;
            let result = sutra::tools::read::handle(&db, &ws.root, &symbol, context_lines, false)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Outline {
            workspace: ws_id,
            path,
        } => {
            let ws_config = workspace::load_workspaces(&config.workspaces_path)?;
            let _ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open(&ws_id, &config.db_dir)?;
            let result = sutra::tools::outline::handle(&db, &path, false)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Impact {
            workspace: ws_id,
            symbol,
        } => {
            let ws_config = workspace::load_workspaces(&config.workspaces_path)?;
            let _ws = workspace::resolve_workspace(&ws_config, &ws_id)?;
            let db = sutra::db::Db::open(&ws_id, &config.db_dir)?;
            let result = sutra::tools::impact::handle(&db, &symbol)?;
            println!("{}", serde_json::to_string(&result)?);
        }
        Commands::Workspaces(cmd) => match cmd {
            WorkspacesCmd::Add {
                id,
                root,
                languages,
            } => {
                workspace::add_workspace(
                    &config.workspaces_path,
                    WorkspaceEntry {
                        id: id.clone(),
                        root: PathBuf::from(root),
                        languages,
                    },
                )?;
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

fn load_workspaces_and_cache(
    config: &Config,
) -> Result<(WsConfig, DbCache), Box<dyn std::error::Error>> {
    let ws_config = workspace::load_workspaces(&config.workspaces_path).unwrap_or_else(|_| {
        workspace::WorkspacesConfig {
            workspace: Vec::new(),
        }
    });
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
    let server = SutraServer::new(config, ws_config, db_cache, parse_coord);
    let (stdin, stdout) = stdio();
    let service = server.serve((stdin, stdout)).await?;

    tokio::select! {
        res = service.waiting() => { res?; }
        _ = shutdown_signal() => { tracing::info!("shutdown signal received"); }
    }
    Ok(())
}

async fn cmd_serve_http(config: Arc<Config>) -> Result<(), Box<dyn std::error::Error>> {
    use axum::routing::any_service;
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager,
        tower::{StreamableHttpServerConfig, StreamableHttpService},
    };
    use sutra::daemon::Daemon;
    use sutra::mcp::SutraServer;
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

    let daemon = Arc::new(Daemon::new(
        Arc::clone(&config),
        Arc::clone(&ws_config),
        Arc::clone(&db_cache),
    ));
    let scheduler_tick = daemon.scheduler_last_tick_handle();
    let dd_engines = daemon.dd_engines();
    let parse_coord = daemon.parse_coordinator();
    let _scheduler = daemon.spawn_scheduler();

    let cancel = CancellationToken::new();
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config.keep_alive = Some(std::time::Duration::from_secs(900));
    let session_manager = Arc::new(session_manager);
    let shttp_config =
        StreamableHttpServerConfig::default().with_cancellation_token(cancel.clone());

    let cfg_clone = config.clone();
    let ws_clone = ws_config.clone();
    let db_clone = db_cache.clone();
    let tick_clone = scheduler_tick.clone();
    let coord_clone = parse_coord.clone();
    let dd_clone = dd_engines.clone();
    let mcp_service = StreamableHttpService::new(
        move || {
            Ok(SutraServer::new(
                cfg_clone.clone(),
                ws_clone.clone(),
                db_clone.clone(),
                coord_clone.clone(),
            )
            .with_scheduler_last_tick(tick_clone.clone())
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
    let daemon_running = tokio::net::TcpStream::connect(addr).await.is_ok();

    let ws_config = workspace::load_workspaces(&config.workspaces_path).unwrap_or_else(|_| {
        workspace::WorkspacesConfig {
            workspace: Vec::new(),
        }
    });

    println!(
        "daemon:     {}",
        if daemon_running {
            "running"
        } else {
            "not running"
        }
    );
    println!("listen:     {addr}");
    println!("workspaces: {}", ws_config.workspace.len());

    for ws in &ws_config.workspace {
        match Db::open(&ws.id, &config.db_dir) {
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
Description=sutra code-intelligence daemon
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
    println!("Reloaded systemd user daemon");

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

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("failed to install CTRL+C handler: {e}");
    }
}
