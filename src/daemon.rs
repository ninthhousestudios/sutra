use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::{Mutex, RwLock};
use tokio::time::{Duration, interval};
use tracing::{info, warn};

use crate::config::Config;
use crate::db::Db;
use crate::pipeline;
use crate::smriti::SmritiReader;
use crate::tools;
use crate::workspace::{WorkspaceEntry, WorkspacesConfig};

pub struct Daemon {
    config: Arc<Config>,
    workspaces: Arc<RwLock<WorkspacesConfig>>,
    db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
    last_watcher_refresh: Arc<Mutex<HashMap<String, Instant>>>,
}

struct DebounceBuffer {
    changed: HashSet<PathBuf>,
    deleted: HashSet<PathBuf>,
    last_event: Instant,
}

struct PollBatch {
    events: Vec<(String, String, PathBuf)>,
    outcome: PollOutcome,
}

enum PollOutcome {
    Ok,
    CursorInvalid,
    ConnectFailed,
}

impl Daemon {
    pub fn new(
        config: Arc<Config>,
        workspaces: Arc<RwLock<WorkspacesConfig>>,
        db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
    ) -> Self {
        Self {
            config,
            workspaces,
            db_cache,
            last_watcher_refresh: Arc::new(Mutex::new(HashMap::new())),
        }
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

    pub fn spawn_smriti_watcher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.smriti_watcher_loop().await;
        })
    }

    async fn smriti_watcher_loop(&self) {
        let poll_dur = Duration::from_secs(self.config.watch_poll_sec);
        let debounce_dur = Duration::from_secs(self.config.watch_debounce_sec);
        let mut tick = interval(poll_dur);
        let mut buffers: HashMap<String, DebounceBuffer> = HashMap::new();

        loop {
            tick.tick().await;

            let entries: Vec<WorkspaceEntry> = self.workspaces.read().workspace.clone();
            let config_db_dir = self.config.db_dir.clone();

            let batch = tokio::task::spawn_blocking(move || {
                poll_smriti_events(&config_db_dir, &entries)
            })
            .await;

            let batch = match batch {
                Ok(b) => b,
                Err(e) => {
                    warn!("smriti poll task panicked: {e}");
                    continue;
                }
            };

            match batch.outcome {
                PollOutcome::ConnectFailed => continue,
                PollOutcome::CursorInvalid => {
                    warn!("smriti cursor invalidated — triggering full reparse");
                    buffers.clear();
                    self.full_reparse_all().await;
                    continue;
                }
                PollOutcome::Ok => {}
            }

            let now = Instant::now();
            for (ws_id, event_type, path) in batch.events {
                let buf = buffers.entry(ws_id).or_insert_with(|| DebounceBuffer {
                    changed: HashSet::new(),
                    deleted: HashSet::new(),
                    last_event: now,
                });
                buf.last_event = now;
                if event_type == "deleted" {
                    buf.deleted.insert(path);
                } else {
                    buf.changed.insert(path);
                }
            }

            self.flush_debounced(&mut buffers, debounce_dur).await;
        }
    }

    async fn flush_debounced(
        &self,
        buffers: &mut HashMap<String, DebounceBuffer>,
        debounce_dur: Duration,
    ) {
        let now = Instant::now();
        let ready_ids: Vec<String> = buffers
            .iter()
            .filter(|(_, buf)| now.duration_since(buf.last_event) >= debounce_dur)
            .map(|(id, _)| id.clone())
            .collect();

        for ws_id in ready_ids {
            let buf = buffers.remove(&ws_id).unwrap();
            if buf.changed.is_empty() && buf.deleted.is_empty() {
                continue;
            }

            let entries: Vec<WorkspaceEntry> = self.workspaces.read().workspace.clone();
            let ws = match entries.into_iter().find(|ws| ws.id == ws_id) {
                Some(ws) => ws,
                None => continue,
            };

            let db = match tools::get_or_open_db(&self.db_cache, &ws.id, &self.config.db_dir) {
                Ok(db) => db,
                Err(e) => {
                    warn!("could not open db for workspace {}: {e}", ws.id);
                    continue;
                }
            };

            let changed: Vec<PathBuf> = buf.changed.into_iter().collect();
            let deleted: Vec<PathBuf> = buf.deleted.into_iter().collect();
            let config = Arc::clone(&self.config);
            let last_refresh = Arc::clone(&self.last_watcher_refresh);
            let ws_id_log = ws.id.clone();

            info!(
                workspace = %ws.id,
                changed = changed.len(),
                deleted = deleted.len(),
                "flushing debounce buffer"
            );

            match tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(
                    pipeline::parse_changed_files(&ws, &db, &config, &changed, &deleted),
                )
            })
            .await
            {
                Ok(Ok(snap)) => {
                    last_refresh.lock().insert(ws_id_log.clone(), Instant::now());
                    info!(
                        "incremental reparse {}: {} files, {} symbols in {}ms",
                        ws_id_log, snap.files_parsed, snap.symbols_extracted, snap.duration_ms
                    );
                }
                Ok(Err(e)) => warn!("incremental reparse failed for {}: {e}", ws_id_log),
                Err(e) => warn!("incremental reparse task panicked for {}: {e}", ws_id_log),
            }
        }
    }

    async fn full_reparse_all(&self) {
        let entries: Vec<WorkspaceEntry> = self.workspaces.read().workspace.clone();
        for ws in &entries {
            let db = match tools::get_or_open_db(&self.db_cache, &ws.id, &self.config.db_dir) {
                Ok(db) => db,
                Err(e) => {
                    warn!("could not open db for workspace {}: {e}", ws.id);
                    continue;
                }
            };

            let ws_clone = ws.clone();
            let db_clone = Arc::clone(&db);
            let config_clone = Arc::clone(&self.config);
            let last_refresh = Arc::clone(&self.last_watcher_refresh);
            let ws_id = ws.id.clone();

            match tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current()
                    .block_on(pipeline::parse_workspace(&ws_clone, &db_clone, &config_clone))
            })
            .await
            {
                Ok(Ok(snap)) => {
                    last_refresh.lock().insert(ws_id.clone(), Instant::now());
                    info!(
                        "full reparse {}: {} files, {} symbols in {}ms",
                        ws_id, snap.files_parsed, snap.symbols_extracted, snap.duration_ms
                    );
                }
                Ok(Err(e)) => warn!("full reparse failed for {}: {e}", ws_id),
                Err(e) => warn!("full reparse task panicked for {}: {e}", ws_id),
            }
        }
    }

    async fn check_stale_workspaces(&self) {
        let entries: Vec<_> = self.workspaces.read().workspace.clone();
        for ws in &entries {
            {
                let refresh_map = self.last_watcher_refresh.lock();
                if let Some(last) = refresh_map.get(&ws.id) {
                    if last.elapsed().as_secs() < self.config.stale_threshold_sec {
                        continue;
                    }
                }
            }

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

fn poll_smriti_events(
    cursor_dir: &std::path::Path,
    workspaces: &[WorkspaceEntry],
) -> PollBatch {
    let reader = match SmritiReader::open(None, cursor_dir) {
        Ok(r) => r,
        Err(_) => {
            return PollBatch {
                events: vec![],
                outcome: PollOutcome::ConnectFailed,
            };
        }
    };

    let ext_set: HashSet<&str> = workspaces
        .iter()
        .flat_map(|ws| ws.languages.iter())
        .flat_map(|lang| pipeline::extensions_for_language(lang))
        .copied()
        .collect();

    let mut cursor = reader.read_cursor();
    let mut events: Vec<(String, String, PathBuf)> = Vec::new();

    loop {
        let page = match reader.events_since(cursor, 500) {
            Ok(p) => p,
            Err(_) => {
                return PollBatch {
                    events: vec![],
                    outcome: PollOutcome::ConnectFailed,
                };
            }
        };

        if !page.cursor_valid {
            reader.write_cursor(0).ok();
            return PollBatch {
                events: vec![],
                outcome: PollOutcome::CursorInvalid,
            };
        }

        for event in &page.events {
            let path = PathBuf::from(&event.path);
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            if !ext_set.contains(ext) {
                continue;
            }
            if let Some(ws) = workspaces.iter().find(|ws| path.starts_with(&ws.root)) {
                events.push((ws.id.clone(), event.event_type.clone(), path));
            }
        }

        cursor = page.next_cursor;
        if !page.has_more {
            break;
        }
    }

    reader.write_cursor(cursor).ok();

    PollBatch {
        events,
        outcome: PollOutcome::Ok,
    }
}
