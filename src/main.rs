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
            let snapshot = sutra::pipeline::parse_workspace(ws, &db, &config).await?;
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
                cmd_guard_install()?;
            }
            GuardCmd::Uninstall => {
                cmd_guard_uninstall()?;
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
    let scheduler_tick = daemon.scheduler_last_tick_handle();
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
    let mcp_service = StreamableHttpService::new(
        move || {
            Ok(
                SutraServer::new(cfg_clone.clone(), ws_clone.clone(), db_clone.clone())
                    .with_scheduler_last_tick(tick_clone.clone()),
            )
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

fn cmd_guard_install() -> Result<(), Box<dyn std::error::Error>> {
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

    // Remove qartez hooks first.
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

    // Remove existing sutra hooks before re-adding.
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

fn cmd_guard_uninstall() -> Result<(), Box<dyn std::error::Error>> {
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

fn find_guard_binary() -> Result<PathBuf, Box<dyn std::error::Error>> {
    // Check common install locations.
    let candidates = [
        dirs::home_dir().map(|h| h.join(".cargo/bin/sutra-guard")),
        Some(PathBuf::from("/usr/local/bin/sutra-guard")),
        dirs::home_dir().map(|h| h.join(".local/bin/sutra-guard")),
    ];
    for candidate in candidates.into_iter().flatten() {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    // Fallback: try which.
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

fn claude_settings_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let home = dirs::home_dir().ok_or("cannot determine home directory")?;
    Ok(home.join(".claude").join("settings.json"))
}

mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

async fn shutdown_signal() {
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("failed to install CTRL+C handler: {e}");
    }
}
