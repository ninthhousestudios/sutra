use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use parking_lot::{Mutex, RwLock};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Config;
use crate::db::Db;
use crate::workspace::WorkspacesConfig;

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    workspaces: Arc<RwLock<WorkspacesConfig>>,
    db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
}

pub fn router(
    config: Arc<Config>,
    workspaces: Arc<RwLock<WorkspacesConfig>>,
    db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
) -> Router {
    let state = AppState {
        config,
        workspaces,
        db_cache,
    };
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/workspaces", post(add_workspace))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn status(State(state): State<AppState>) -> Json<Value> {
    let ws_config = state.workspaces.read();
    let db_cache = state.db_cache.lock();

    let workspaces: Vec<Value> = ws_config
        .workspace
        .iter()
        .map(|entry| {
            let (file_count, symbol_count, last_parse_time) =
                if let Some(db) = db_cache.get(&entry.id) {
                    let files = db.all_files().unwrap_or_default();
                    let sym_counts = db.symbol_counts_by_file().unwrap_or_default();
                    let total_symbols: i64 = sym_counts.values().sum();
                    let last_parse = db.last_parse_time().ok().flatten().unwrap_or_default();
                    (files.len() as i64, total_symbols, last_parse)
                } else {
                    (0, 0, String::new())
                };

            json!({
                "id": entry.id,
                "root": entry.root.display().to_string(),
                "languages": entry.languages,
                "file_count": file_count,
                "symbol_count": symbol_count,
                "last_parse_time": last_parse_time,
            })
        })
        .collect();

    Json(json!({ "workspaces": workspaces }))
}

#[derive(Deserialize)]
struct AddWorkspaceRequest {
    path: String,
    languages: Option<Vec<String>>,
}

async fn add_workspace(
    State(state): State<AppState>,
    Json(req): Json<AddWorkspaceRequest>,
) -> impl IntoResponse {
    let root = PathBuf::from(&req.path);
    let root = root.canonicalize().unwrap_or(root);
    let id = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| req.path.clone());

    let languages = req
        .languages
        .unwrap_or_else(|| vec!["rust".into(), "dart".into()]);

    let entry = crate::workspace::WorkspaceEntry {
        id: id.clone(),
        root: root.clone(),
        languages: languages.clone(),
    };

    if let Err(e) = crate::workspace::add_workspace(&state.config.workspaces_path, entry.clone()) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"id": id, "status": "already_registered", "error": e.to_string()})),
        );
    }

    {
        let mut ws = state.workspaces.write();
        ws.workspace.push(entry);
    }

    if let Ok(db) = Db::open(&id, &state.config.db_dir) {
        state.db_cache.lock().insert(id.clone(), Arc::new(db));
    }

    (
        StatusCode::OK,
        Json(json!({"id": id, "status": "registered"})),
    )
}
