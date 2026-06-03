use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use sutra::config::Config;
use sutra::db::{Db, SnapshotParams};
use sutra::workspace::{WorkspaceEntry, WorkspacesConfig};

type DbCache = Arc<Mutex<HashMap<String, Arc<Db>>>>;
type WsConfig = Arc<RwLock<WorkspacesConfig>>;

fn test_config() -> Arc<Config> {
    Arc::new(Config::test_default())
}

fn test_state() -> (Arc<Config>, WsConfig, DbCache) {
    let config = test_config();
    let ws = Arc::new(RwLock::new(WorkspacesConfig { workspace: vec![] }));
    let db_cache: DbCache = Arc::new(Mutex::new(HashMap::new()));
    (config, ws, db_cache)
}

fn test_app() -> axum::Router {
    let (config, ws, db_cache) = test_state();
    sutra::rest::router(config, ws, db_cache)
}

#[tokio::test]
async fn health_returns_ok() {
    let app = test_app();
    let resp = app
        .oneshot(Request::get("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
}

#[tokio::test]
async fn status_returns_workspace_list() {
    let (config, ws, db_cache) = test_state();

    let dir = tempfile::tempdir().unwrap();
    let db = Db::open_unchecked("test_ws", dir.path()).unwrap();
    db.insert_snapshot(&SnapshotParams {
        files_parsed: 1,
        symbols_extracted: 5,
        refs_extracted: 20,
        parse_errors: 0,
        duration_ms: 100,
        total_complexity: 0,
        dead_symbol_count: 0,
        hotspot_count: 0,
        health_score: 0,
    })
    .unwrap();

    {
        let mut ws_guard = ws.write();
        ws_guard.workspace.push(WorkspaceEntry {
            id: "test_ws".into(),
            root: "/tmp/test_ws".into(),
            languages: vec!["rust".into()],
        });
    }
    db_cache.lock().insert("test_ws".into(), Arc::new(db));

    let app = sutra::rest::router(config, ws, db_cache);
    let resp = app
        .oneshot(Request::get("/status").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let workspaces = json["workspaces"]
        .as_array()
        .expect("must have workspaces array");
    assert_eq!(workspaces.len(), 1);

    let ws0 = &workspaces[0];
    assert_eq!(ws0["id"], "test_ws");
    assert_eq!(ws0["root"], "/tmp/test_ws");
    assert!(
        ws0["last_parse_time"].is_string(),
        "must have last_parse_time"
    );
    assert!(ws0["file_count"].is_number(), "must have file_count");
    assert!(ws0["symbol_count"].is_number(), "must have symbol_count");
}

#[tokio::test]
async fn post_workspaces_registers_new_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let ws_path = dir.path().join("workspaces.toml");
    let config = Arc::new(Config {
        db_dir: dir.path().to_path_buf(),
        workspaces_path: ws_path,
        listen_addr: "127.0.0.1:0".into(),
        parse_parallelism: 1,
        stale_threshold_sec: 600,
        log_level: "warn".into(),
        constraints_idle_timeout_sec: 1800,
        parse_timeout_ms: 5000,
    });
    let ws: WsConfig = Arc::new(RwLock::new(WorkspacesConfig { workspace: vec![] }));
    let db_cache: DbCache = Arc::new(Mutex::new(HashMap::new()));

    let app = sutra::rest::router(config.clone(), ws.clone(), db_cache.clone());

    let body = serde_json::json!({
        "path": "/tmp/my-project",
        "languages": ["rust"]
    });

    let resp = app
        .oneshot(
            Request::post("/workspaces")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], "my-project");
    assert_eq!(json["status"], "registered");

    // workspace should be in the in-memory config
    let ws_guard = ws.read();
    assert_eq!(ws_guard.workspace.len(), 1);
    assert_eq!(ws_guard.workspace[0].id, "my-project");

    // workspace should be persisted to disk
    let persisted = sutra::workspace::load_workspaces(&config.workspaces_path).unwrap();
    assert_eq!(persisted.workspace.len(), 1);
}
