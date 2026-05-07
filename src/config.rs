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
    pub watch_poll_sec: u64,
    pub watch_debounce_sec: u64,
    pub log_level: String,
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

        let listen_addr = std::env::var("SUTRA_LISTEN_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:3201".to_string());

        let parse_parallelism = parse_env_or("SUTRA_PARSE_PARALLELISM", num_cpus());
        let stale_threshold_sec = parse_env_or("SUTRA_STALE_THRESHOLD_SEC", 600u64);
        let watch_poll_sec = parse_env_or("SUTRA_WATCH_POLL_SEC", 2u64);
        let watch_debounce_sec = parse_env_or("SUTRA_WATCH_DEBOUNCE_SEC", 3u64);
        let log_level =
            std::env::var("SUTRA_LOG_LEVEL").unwrap_or_else(|_| "info".to_string());

        Ok(Self {
            db_dir,
            workspaces_path,
            listen_addr,
            parse_parallelism,
            stale_threshold_sec,
            watch_poll_sec,
            watch_debounce_sec,
            log_level,
        })
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
