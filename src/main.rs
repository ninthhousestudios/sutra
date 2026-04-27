use std::sync::Arc;

use clap::{Parser, Subcommand};
use sutra::config::Config;

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
        Commands::Serve { stdio: _ } => {
            todo!("serve command — Issue 7")
        }
        Commands::Parse { workspace: _ } => {
            todo!("parse command — Issue 5")
        }
        Commands::Workspaces(cmd) => match cmd {
            WorkspacesCmd::Add {
                id: _,
                root: _,
                languages: _,
            } => todo!("workspaces add — Issue 3"),
            WorkspacesCmd::List => todo!("workspaces list — Issue 3"),
            WorkspacesCmd::Remove { id: _ } => todo!("workspaces remove — Issue 3"),
        },
        Commands::Health => {
            todo!("health command — Issue 7")
        }
    }

    #[allow(unreachable_code)]
    Ok(())
}
