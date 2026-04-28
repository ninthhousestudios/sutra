use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use tokio::time::{Duration, interval};
use tracing::{info, warn};

use crate::config::Config;
use crate::db::Db;
use crate::pipeline;
use crate::tools;
use crate::workspace::WorkspacesConfig;

pub struct Daemon {
    config: Arc<Config>,
    workspaces: Arc<RwLock<WorkspacesConfig>>,
    db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
}

impl Daemon {
    pub fn new(
        config: Arc<Config>,
        workspaces: Arc<RwLock<WorkspacesConfig>>,
        db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
    ) -> Self {
        Self { config, workspaces, db_cache }
    }

    pub fn spawn_scheduler(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(this.config.stale_threshold_sec / 2));
            loop {
                tick.tick().await;
                this.check_stale_workspaces().await;
            }
        })
    }

    async fn check_stale_workspaces(&self) {
        let entries: Vec<_> = self.workspaces.read().workspace.clone();
        for ws in &entries {
            let db = match tools::get_or_open_db(&self.db_cache, &ws.id, &self.config.db_dir) {
                Ok(db) => db,
                Err(e) => {
                    warn!("could not open db for workspace {}: {e}", ws.id);
                    continue;
                }
            };

            let is_stale = match db.last_parse_time() {
                Ok(Some(ts)) => chrono::DateTime::parse_from_rfc3339(&ts)
                    .map(|dt| {
                        let age = chrono::Utc::now() - dt.with_timezone(&chrono::Utc);
                        age.num_seconds() as u64 > self.config.stale_threshold_sec
                    })
                    .unwrap_or(true),
                Ok(None) => true,
                Err(_) => true,
            };

            if is_stale {
                info!("workspace {} is stale, triggering reparse", ws.id);
                let ws_clone = ws.clone();
                let db_clone = Arc::clone(&db);
                let config_clone = Arc::clone(&self.config);
                let ws_id = ws.id.clone();
                match tokio::task::spawn_blocking(move || {
                    tokio::runtime::Handle::current()
                        .block_on(pipeline::parse_workspace(&ws_clone, &db_clone, &config_clone))
                })
                .await
                {
                    Ok(Ok(snap)) => {
                        info!(
                            "reparsed {}: {} files, {} symbols in {}ms",
                            ws_id, snap.files_parsed, snap.symbols_extracted, snap.duration_ms
                        );
                    }
                    Ok(Err(e)) => {
                        warn!("reparse failed for {}: {e}", ws_id);
                    }
                    Err(e) => {
                        warn!("reparse task panicked for {}: {e}", ws_id);
                    }
                }
            }
        }
    }
}
