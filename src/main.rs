use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use parking_lot::{Mutex, RwLock};
use sutra::config::Config;
use sutra::db::Db;
use sutra::workspace::{self, WorkspaceEntry};

#[derive(Parser)]
#[command(name = "sutra", about = "Code intelligence for manas")]
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
    /// Check daemon and database health
    Health,
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

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&config.log_level)),
        )
        .init();

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
            let snapshot = sutra::pipeline::parse_workspace(ws, &db, &config).await?;
            let resolvable = snapshot.refs_extracted - snapshot.skipped_count;
            let resolved = resolvable - snapshot.unresolved_count;
            let pct = if resolvable > 0 { resolved * 100 / resolvable } else { 0 };
            println!(
                "Parsed {} files, {} symbols, {} refs ({} resolved of {} resolvable, {}%; {} skipped) in {}ms",
                snapshot.files_parsed,
                snapshot.symbols_extracted,
                snapshot.refs_extracted,
                resolved,
                resolvable,
                pct,
                snapshot.skipped_count,
                snapshot.duration_ms
            );
        }
        Commands::Workspaces(cmd) => match cmd {
            WorkspacesCmd::Add { id, root, languages } => {
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
        Commands::Health => {
            cmd_health(&config).await?;
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
    let server = SutraServer::new(config, ws_config, db_cache);
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
    let mcp_service = StreamableHttpService::new(
        move || Ok(SutraServer::new(cfg_clone.clone(), ws_clone.clone(), db_clone.clone())),
        session_manager,
        shttp_config,
    );

    #[allow(deprecated)]
    let app = axum::Router::new().route("/mcp", any_service(mcp_service));

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
        if daemon_running { "running" } else { "not running" }
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

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("failed to install CTRL+C handler: {e}");
    }
}
