use std::path::PathBuf;
use std::str::FromStr;

use crate::error::Result;

#[derive(Debug, Clone)]
pub struct Config {
    pub db_dir: PathBuf,
    pub workspaces_path: PathBuf,
    pub listen_addr: String,
    pub parse_parallelism: usize,
    pub stale_threshold_sec: u64,
    pub log_level: String,
    pub dd_idle_timeout_sec: u64,
    pub parse_timeout_ms: u64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let db_dir = std::env::var("SUTRA_DB_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_db_dir());

        let workspaces_path = std::env::var("SUTRA_WORKSPACES")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let mut p = db_dir.clone();
                p.push("workspaces.toml");
                p
            });

        let listen_addr =
            std::env::var("SUTRA_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:3201".to_string());

        let parse_parallelism = parse_env_or("SUTRA_PARSE_PARALLELISM", num_cpus());
        let stale_threshold_sec = parse_env_or("SUTRA_STALE_THRESHOLD_SEC", 600u64);
        let log_level = std::env::var("SUTRA_LOG_LEVEL").unwrap_or_else(|_| "info".to_string());
        let dd_idle_timeout_sec = parse_env_or("SUTRA_DD_IDLE_TIMEOUT_SEC", 1800u64);
        let parse_timeout_ms = parse_env_or("SUTRA_PARSE_TIMEOUT_MS", 5000u64);

        Ok(Self {
            db_dir,
            workspaces_path,
            listen_addr,
            parse_parallelism,
            stale_threshold_sec,
            log_level,
            dd_idle_timeout_sec,
            parse_timeout_ms,
        })
    }
}

impl Config {
    pub fn test_default() -> Self {
        Self {
            db_dir: PathBuf::from("/tmp/sutra-test"),
            workspaces_path: PathBuf::from("/tmp/sutra-test/workspaces.toml"),
            listen_addr: "127.0.0.1:0".into(),
            parse_parallelism: 1,
            stale_threshold_sec: 600,
            log_level: "warn".into(),
            dd_idle_timeout_sec: 1800,
            parse_timeout_ms: 5000,
        }
    }
}

fn default_db_dir() -> PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        let mut p = PathBuf::from(home);
        p.push(".sutra");
        p
    } else {
        PathBuf::from(".sutra")
    }
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

fn parse_env_or<T: FromStr + std::fmt::Display>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Err(_) => default,
        Ok(v) => match v.parse() {
            Ok(parsed) => parsed,
            Err(_) => {
                eprintln!(
                    "WARNING: {name}={v:?} is not a valid {ty} — using default {default}",
                    ty = std::any::type_name::<T>(),
                );
                default
            }
        },
    }
}
