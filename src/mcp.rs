use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::Config;
use crate::constraints::DdEngine;
use crate::db::Db;
use crate::error::SutraError;
use crate::guard;
use crate::lessons::LessonsDb;
use crate::pipeline::ParseCoordinator;
use crate::tools;
use crate::workspace::{self, WorkspacesConfig};

// ---------------------------------------------------------------------------
// Args structs (local — no tool module)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HelpArgs {
    /// Topic name (omit for topic list)
    #[serde(default)]
    pub topic: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkspaceToolArgs {
    /// Absolute path to workspace root. Required for status and reparse actions; omit for tier-only operations.
    #[serde(default)]
    pub path: Option<String>,
    /// Action: "status" (default, register + return health/counts) or "reparse" (synchronous reparse)
    #[serde(default)]
    pub action: Option<String>,
    /// Languages to index (default: ["rust", "dart"])
    #[serde(default)]
    pub languages: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Args structs re-exported from tool modules
// ---------------------------------------------------------------------------

use crate::tools::calls::CallsArgs;
use crate::tools::cochange::CochangeArgs;
use crate::tools::commit_manifest::CommitManifestArgs;
use crate::tools::components::ComponentsArgs;
use crate::tools::context::ContextArgs;
use crate::tools::dead::DeadArgs;
use crate::tools::deps::DepsArgs;
use crate::tools::diff_impact::DiffImpactArgs;
use crate::tools::explore::ExploreArgs;
use crate::tools::file_health::FileHealthArgs;
use crate::tools::hotspots::HotspotsArgs;
use crate::tools::impact::ImpactArgs;
use crate::tools::lookup::LookupArgs;
use crate::tools::map::MapArgs;
use crate::tools::outline::OutlineArgs;
use crate::tools::pr_risk::PrRiskArgs;
use crate::tools::provenance::ProvenanceArgs;
use crate::tools::read::ReadArgs;
use crate::tools::refs::RefsArgs;
use crate::tools::remember::RememberArgs;
use crate::tools::review::ReviewArgs;
use crate::tools::similar::SimilarArgs;
use crate::tools::trace::TraceArgs;
use crate::tools::trend::TrendArgs;
use crate::tools::winnow::WinnowArgs;

// ---------------------------------------------------------------------------
// SutraServer
// ---------------------------------------------------------------------------

pub struct SutraServer {
    db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
    config: Arc<Config>,
    workspaces: Arc<RwLock<WorkspacesConfig>>,
    parse_coord: ParseCoordinator,
    dd_engines: Arc<Mutex<HashMap<String, Arc<DdEngine>>>>,
    lessons_db: Arc<LessonsDb>,
    /// Canonical id of the workspace to use when a tool call omits `workspace`
    /// (empty string). Set only on the stdio path, where the server process
    /// shares the client session's CWD; `None` under the http daemon, which
    /// serves many repos and has no single "current" workspace.
    default_workspace: Option<String>,
    tool_router: ToolRouter<Self>,
}

impl Clone for SutraServer {
    fn clone(&self) -> Self {
        Self {
            db_cache: Arc::clone(&self.db_cache),
            config: Arc::clone(&self.config),
            workspaces: Arc::clone(&self.workspaces),
            parse_coord: self.parse_coord.clone(),
            dd_engines: Arc::clone(&self.dd_engines),
            lessons_db: Arc::clone(&self.lessons_db),
            default_workspace: self.default_workspace.clone(),
            tool_router: Self::tool_router(),
        }
    }
}

impl SutraServer {
    pub fn new(
        config: Arc<Config>,
        workspaces: Arc<RwLock<WorkspacesConfig>>,
        db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
        parse_coord: ParseCoordinator,
        lessons_db: Arc<LessonsDb>,
    ) -> Self {
        Self {
            db_cache,
            config,
            workspaces,
            parse_coord,
            dd_engines: Arc::new(Mutex::new(HashMap::new())),
            lessons_db,
            default_workspace: None,
            tool_router: Self::tool_router(),
        }
    }

    pub fn with_dd_engines(mut self, engines: Arc<Mutex<HashMap<String, Arc<DdEngine>>>>) -> Self {
        self.dd_engines = engines;
        self
    }

    /// Set the workspace used when a tool call omits `workspace`. Passed the
    /// canonical id of the CWD workspace on the stdio path; a no-op default of
    /// `None` keeps the http daemon explicit-workspace-only.
    pub fn with_default_workspace(mut self, ws_id: Option<String>) -> Self {
        self.default_workspace = ws_id;
        self
    }

    fn get_dd_engine(&self, ws_id: &str) -> Arc<DdEngine> {
        let ws_id = self.canonical_ws_id(ws_id);
        let mut engines = self.dd_engines.lock();
        Arc::clone(engines.entry(ws_id).or_insert_with(|| {
            Arc::new(DdEngine::new(Duration::from_secs(
                self.config.constraints_idle_timeout_sec,
            )))
        }))
    }

    fn resolve_workspace(
        &self,
        ws_id: &str,
    ) -> std::result::Result<crate::workspace::WorkspaceEntry, ErrorData> {
        // Empty `workspace` means "use the session default" — the CWD workspace
        // on the stdio path. With no default (http daemon, or CWD outside any
        // registered workspace) this is a clear error rather than a silent guess.
        let ws_id = if ws_id.trim().is_empty() {
            match self.default_workspace.as_deref() {
                Some(id) => id,
                None => {
                    return Err(ErrorData::new(
                        rmcp::model::ErrorCode(crate::error::codes::INVALID_PARAMS),
                        "no `workspace` given and no session default (the server's CWD is \
                         not inside a registered workspace); pass `workspace` explicitly"
                            .to_string(),
                        None,
                    ));
                }
            }
        } else {
            ws_id
        };
        workspace::resolve_workspace(&self.workspaces.read(), ws_id)
            .cloned()
            .map_err(sutra_to_rmcp)
    }

    /// Canonical workspace id for a raw arg, applying the session default for an
    /// empty arg. Infallible: helpers that *key* on the id (parse lock, DD
    /// engine map) must land on the same canonical id `resolve_workspace`
    /// resolves to — path/basename/empty alike — or they desync onto a stray
    /// key. On resolution failure it degrades to the raw input (prior behavior).
    fn canonical_ws_id(&self, ws_id: &str) -> String {
        self.resolve_workspace(ws_id)
            .map(|w| w.id)
            .unwrap_or_else(|_| ws_id.to_string())
    }

    fn get_db(&self, ws_id: &str) -> std::result::Result<Arc<Db>, ErrorData> {
        let ws = self.resolve_workspace(ws_id)?;
        tools::get_or_open_db(&self.db_cache, &ws, &self.config.db_dir).map_err(sutra_to_rmcp)
    }

    async fn tool_context(
        &self,
        ws_id: &str,
    ) -> std::result::Result<tools::ToolContext, ErrorData> {
        let ws = self.resolve_workspace(ws_id)?;
        let db = tools::get_or_open_db(&self.db_cache, &ws, &self.config.db_dir)
            .map_err(sutra_to_rmcp)?;
        // Refresh before answering (sutra/363): if the workspace drifted, take
        // the parse lock and incrementally reparse the drift set so the query
        // below reads the edit. The freshness probe then re-runs and reports
        // clean. Never fatal — a failure returns a note and we answer as-is.
        // A second cheap resolve hands `refresh_before_answer` an owned entry to
        // move across the spawn_blocking boundary without a value clone.
        let refresh_note = self
            .refresh_before_answer(&db, self.resolve_workspace(ws_id)?)
            .await;
        let mut response_freshness = self.freshness(&db, &ws.root);
        if let Some(note) = refresh_note {
            response_freshness["refresh"] = serde_json::Value::String(note);
        }
        Ok(tools::ToolContext::new(
            db,
            ws.root,
            true,
            response_freshness,
        ))
    }

    /// Wait at most this long for an in-flight parse before answering from the
    /// current index. A one-file incremental reparse finishes well under this;
    /// exceeding it means a large parse (full or first parse) holds the lock,
    /// and blocking the query on it is worse than a one-cycle-stale answer.
    const REFRESH_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

    /// Refresh the index before answering (sutra/363, sutra/382): probe drift
    /// and the parser stamp; if the workspace moved, incrementally reparse the
    /// drift set under the parse lock, and if the extractor changed, run a full
    /// re-extraction instead. Returns an optional envelope note. Never fatal —
    /// every failure mode (lock contention past the deadline, parse error/panic)
    /// degrades to answering from the current index; `is_stale` in the envelope
    /// still reflects reality (and the stamp heal runs *before* the caller reads
    /// freshness, so a healed workspace reports honestly).
    async fn refresh_before_answer(
        &self,
        db: &Arc<Db>,
        entry: workspace::WorkspaceEntry,
    ) -> Option<String> {
        // Frozen index is immutable — never refreshed on the query path.
        if entry.frozen {
            return None;
        }

        // The stored index may predate this binary's extractor (sutra/382): a
        // parser change (grammar bump, adapter fix, symbol-kind change) leaves
        // the bytes clean but the symbols stale, so content drift alone never
        // flags it. Only a full re-extraction heals it — the incremental drift
        // path deliberately does not (it would reparse only the drift set) — so
        // a mismatch forces a full `parse_workspace` here. The query path is the
        // single choke point every transport (stdio + http) and every workspace
        // (CWD or not) funnels through; the startup reparse only covers the stdio
        // CWD workspace, so http and non-CWD workspaces would otherwise never
        // heal and would serve stale symbols while reporting `is_stale: false`.
        let stamp_mismatch =
            db.parser_stamp().unwrap_or(None).as_deref() != Some(crate::parser::PARSER_STAMP);

        // Probe drift. A current stamp with no drift → answer immediately, zero
        // cost beyond the probe. No baseline (never parsed / read failure) → not
        // something the query path should fix (a stamp heal needs a prior index
        // to re-extract); leave it to startup / explicit reparse.
        let drifted = match crate::freshness::workspace_drift(db, &entry.root, &entry.languages) {
            (_, Some(d)) => !d.is_empty(),
            (_, None) => return None,
        };
        if !drifted && !stamp_mismatch {
            return None;
        }

        // Need a reparse: serialize behind the parse lock with a bounded wait. If
        // a parse already holds it past the deadline, answer from the current
        // index rather than blocking the query on a big rebuild.
        let lock = self.parse_coord.lock_for(&entry.id);
        let guard = match tokio::time::timeout(Self::REFRESH_LOCK_WAIT, lock.lock_owned()).await {
            Ok(g) => g,
            Err(_) => {
                return Some(
                    "a reparse is in flight, answering from the current index".to_string(),
                );
            }
        };

        // Re-check under the lock: a concurrent refresh may have healed the stamp
        // and/or cleaned the drift, so N racing queries produce one parse, not N.
        let stamp_mismatch =
            db.parser_stamp().unwrap_or(None).as_deref() != Some(crate::parser::PARSER_STAMP);
        let drift = match crate::freshness::workspace_drift(db, &entry.root, &entry.languages) {
            (_, Some(d)) if !d.is_empty() => Some(d),
            _ => None,
        };
        if drift.is_none() && !stamp_mismatch {
            return None;
        }

        let mark = self.parse_coord.mark_parsing(&entry.id);
        let db = Arc::clone(db);
        let entry = Arc::new(entry);
        let entry_bg = Arc::clone(&entry);
        let config = Arc::clone(&self.config);
        // Move the parse lock guard and mark INTO the blocking task so their
        // lifetime tracks the write, not this async future. spawn_blocking is not
        // cancelled when the awaiting future is dropped (HTTP client disconnect /
        // request cancellation); holding them out here would release the lock the
        // instant the future drops while the parse keeps mutating SQLite, letting
        // a concurrent DD read (`hold_parse_lock`) or refresh enter mid-write and
        // read a half-written index (sutra/380).
        let result = tokio::task::spawn_blocking(move || {
            let _guard = guard;
            let _mark = mark;
            let registry = crate::parser::adapter::default_registry();
            if stamp_mismatch {
                // Extractor changed: a full walk re-extracts every file and
                // re-records the stamp, and subsumes any pending content drift.
                // A persistently failing file leaves the stamp unadvanced
                // (sutra/381), so this can re-run per query until the file heals;
                // the parse lock caps that to one walk at a time, which is
                // preferable to silently serving stale symbols.
                let cancel = std::sync::atomic::AtomicBool::new(false);
                crate::pipeline::parse_workspace(&entry_bg, &db, &config, &cancel, &registry)
            } else {
                // Hot path: only the drifted files.
                crate::pipeline::parse_incremental(
                    &entry_bg,
                    &db,
                    &config,
                    &registry,
                    drift
                        .as_ref()
                        .expect("drift is Some when the stamp is current"),
                )
            }
        })
        .await;

        match result {
            Ok(Ok(snap)) => {
                tracing::debug!(
                    files = snap.files_parsed,
                    ms = snap.duration_ms,
                    full = stamp_mismatch,
                    "query-path refresh complete"
                );
                None
            }
            Ok(Err(e)) => {
                tracing::warn!("query-path refresh failed: {e}");
                Some("index refresh failed, answering from the current index".to_string())
            }
            Err(e) => {
                tracing::warn!("query-path refresh panicked: {e}");
                Some("index refresh failed, answering from the current index".to_string())
            }
        }
    }

    fn register_workspace(
        &self,
        path: &str,
        languages: Option<Vec<String>>,
    ) -> std::result::Result<(String, workspace::WorkspaceEntry, bool), ErrorData> {
        let root = PathBuf::from(path);
        if !root.is_absolute() || !root.is_dir() {
            return Err(ErrorData::new(
                rmcp::model::ErrorCode(crate::error::codes::INVALID_PARAMS),
                format!("path must be an absolute directory that exists: {path}"),
                None,
            ));
        }
        let root = root.canonicalize().map_err(|e| {
            ErrorData::new(
                rmcp::model::ErrorCode(crate::error::codes::INVALID_PARAMS),
                format!("cannot canonicalize path {path}: {e}"),
                None,
            )
        })?;

        // If this exact directory is already registered — possibly under a
        // custom id that differs from the basename — reuse that entry. Deriving
        // a fresh basename id and re-adding would trip add_workspace's
        // root-overlap guard against the existing workspace (it overlaps
        // itself), which made status/reparse unreachable for custom-id
        // workspaces (sutra/355).
        if let Some(existing) = workspace::find_by_root(&self.workspaces.read(), &root) {
            return Ok((existing.id.clone(), existing.clone(), true));
        }

        let dir_name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let ws_id = dir_name.to_lowercase().replace(' ', "-");
        let languages = languages.unwrap_or_else(|| vec!["rust".into(), "dart".into()]);

        let entry = workspace::WorkspaceEntry {
            id: ws_id.clone(),
            root,
            languages,
            frozen: false,
        };
        workspace::validate_db_dir_for_workspace(&self.config.db_dir, &entry)
            .map_err(sutra_to_rmcp)?;

        let already_exists = {
            let mut config = self.workspaces.write();
            let exists = config.workspace.iter().any(|w| w.id == ws_id);
            if !exists {
                workspace::add_workspace(&self.config.workspaces_path, entry.clone())
                    .map_err(sutra_to_rmcp)?;
                config.workspace.push(entry.clone());
            }
            exists
        };

        Ok((ws_id, entry, already_exists))
    }

    fn freshness(&self, db: &Db, workspace_root: &Path) -> serde_json::Value {
        let ws_guard = self.workspaces.read();
        let entry = workspace::resolve_workspace(&ws_guard, db.workspace_id()).ok();
        let frozen = entry.map(|e| e.frozen).unwrap_or(false);
        let languages: &[String] = entry.map(|e| e.languages.as_slice()).unwrap_or(&[]);

        // A frozen workspace has an immutable index; it is never stale for action
        // purposes, so skip the drift probe entirely (its corpus may be huge) and
        // just report the last parse timestamp. Otherwise the probe scopes its
        // walk to the workspace's indexed languages, so a docs-only change is
        // never mistaken for an added source file.
        let (as_of, is_stale) = if frozen {
            (db.last_parse_info().ok().flatten().map(|(ts, _)| ts), false)
        } else {
            crate::freshness::is_workspace_stale(db, workspace_root, languages)
        };
        drop(ws_guard);

        let parsing = self.parse_coord.is_parsing(db.workspace_id());

        let mut val = serde_json::json!({
            "as_of": as_of,
            "is_stale": is_stale,
            // Echo which workspace answered, so an omitted `workspace` that
            // defaulted to an unexpected repo is visible in the response.
            "workspace": db.workspace_id(),
        });
        if frozen {
            val["frozen"] = serde_json::Value::Bool(true);
        }
        if parsing {
            val["parsing_in_progress"] = serde_json::Value::Bool(true);
        }
        val
    }

    async fn await_parse(&self, ws_id: &str) {
        // Best-effort wait, not a correctness gate: `canonical_ws_id` degrades an
        // unresolvable empty arg (no session default) to a "" lock key rather than
        // erroring like `hold_parse_lock`. Harmless — the request's `get_db` errors
        // out regardless, so the "" lock is acquired and dropped for nothing.
        let lock = self.parse_coord.lock_for(&self.canonical_ws_id(ws_id));
        let _guard = lock.lock().await;
    }

    /// Acquire the per-workspace parse lock and *hold* it across a DD-backed
    /// evaluation. A reparse remints file ids and commits per file; if it lands
    /// between the `all_files()` read (→ path_map) and the `import_edges()` read
    /// the engine syncs to, path_map and the edges end up in disjoint id spaces,
    /// reviving the silent-clean of sutra/297 (and, per sutra/299, can hang the
    /// shared engine). Holding this guard makes evaluation and reparse mutually
    /// exclusive, closing the window at the source; the read-side data_generation
    /// guard in `constraints::check::evaluate` stays as belt-and-suspenders.
    ///
    /// The lock is keyed on the *canonical* workspace id — the same key the
    /// `reparse` action and the first-parse task use — so callers may pass a
    /// path or basename and still land on the one mutex that serializes parses.
    async fn hold_parse_lock(
        &self,
        ws_id: &str,
    ) -> std::result::Result<tokio::sync::OwnedMutexGuard<()>, ErrorData> {
        let entry = self.resolve_workspace(ws_id)?;
        let lock = self.parse_coord.lock_for(&entry.id);
        Ok(lock.lock_owned().await)
    }

    /// Demand health refresh (sutra/415 Wave C): the *acquiring* adapter for
    /// health consumers (file health, workspace summaries). Acquires the parse
    /// coordinator with the same bounded wait as the query path, then a
    /// *nonblocking* cross-process flock, and drives the shared refresh core.
    /// It never blocks a health query behind a peer's write (contention defers),
    /// and only health consumers call it — ordinary symbol queries must NOT
    /// rebuild health. Review does not use this adapter: it already holds the
    /// coordinator and would deadlock re-acquiring it; it calls the locked core.
    ///
    /// The coordinator guard and the flock live inside `spawn_blocking` so a
    /// dropped future (HTTP disconnect) cannot release them mid-write, exactly as
    /// `refresh_before_answer` does for the incremental reparse (sutra/380).
    async fn refresh_health(&self, ws_id: &str) -> crate::health::refresh::DemandOutcome {
        use crate::health::evidence::DeferReason;
        use crate::health::refresh::DemandOutcome;

        let entry = match self.resolve_workspace(ws_id) {
            Ok(e) => e,
            Err(_) => return DemandOutcome::Failed,
        };
        // A frozen index is immutable: it can serve retained evidence but must not
        // assert current filesystem health without validation (contract).
        if entry.frozen {
            return DemandOutcome::Deferred(DeferReason::Frozen);
        }
        let db = match self.get_db(ws_id) {
            Ok(d) => d,
            Err(_) => return DemandOutcome::Failed,
        };

        // In-process serialization: the same bounded wait the query path uses. A
        // parse holding it past the deadline means defer rather than block.
        let lock = self.parse_coord.lock_for(&entry.id);
        let guard = match tokio::time::timeout(Self::REFRESH_LOCK_WAIT, lock.lock_owned()).await {
            Ok(g) => g,
            Err(_) => return DemandOutcome::Deferred(DeferReason::LockBusy),
        };

        let config = Arc::clone(&self.config);
        // Move the owned entry fields into the worker (no clone): `entry` is a
        // local and its id was only borrowed above to key the coordinator lock.
        let root = entry.root;
        let ws_lock_id = entry.id;
        let now = chrono::Utc::now().timestamp();
        let result = tokio::task::spawn_blocking(move || {
            // Hold the coordinator guard for the whole write; refresh_acquiring
            // takes the nonblocking cross-process flock and drives the core.
            let _guard = guard;
            match crate::health::refresh::refresh_acquiring(&config, &ws_lock_id, &db, &root, now) {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!("health: demand refresh failed: {e}");
                    DemandOutcome::Failed
                }
            }
        })
        .await;

        match result {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!("health: demand refresh worker panicked: {e}");
                DemandOutcome::Failed
            }
        }
    }

    /// Refresh persistent health under an ALREADY-HELD coordinator lock (the
    /// review DD path). Uses the shared locked core directly — never the acquiring
    /// [`Self::refresh_health`], which would re-lock the coordinator and deadlock.
    /// Takes only the nonblocking cross-process flock; on contention or error the
    /// prior run stands. Best-effort and synchronous: the caller runs the DD
    /// evaluation inline under the same guard with no await in between, so this
    /// cannot be cancelled mid-write.
    fn refresh_health_locked(
        &self,
        db: &Db,
        root: &Path,
        ws_id: &str,
    ) -> crate::health::refresh::DemandOutcome {
        use crate::health::refresh::DemandOutcome;
        let canonical = self.canonical_ws_id(ws_id);
        let now = chrono::Utc::now().timestamp();
        match crate::health::refresh::refresh_acquiring(&self.config, &canonical, db, root, now) {
            Ok(o) => o,
            Err(e) => {
                tracing::warn!("review: health refresh failed: {e}");
                DemandOutcome::Failed
            }
        }
    }

    fn wrap_response(
        &self,
        db: &Db,
        workspace_root: &Path,
        mut result: serde_json::Value,
    ) -> std::result::Result<String, ErrorData> {
        if let Some(obj) = result.as_object_mut() {
            let f = self.freshness(db, workspace_root);
            if let Some(f_obj) = f.as_object() {
                for (k, v) in f_obj {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        to_compact_json(result)
    }
}

// ---------------------------------------------------------------------------
// Tool methods
// ---------------------------------------------------------------------------

#[tool_router(router = tool_router)]
impl SutraServer {
    #[tool(description = "Health check across all registered workspaces. \
        Returns per-workspace file/symbol counts, parse errors, and staleness.")]
    pub async fn sutra_health(
        &self,
        #[allow(unused_variables)] Parameters(_args): Parameters<EmptyArgs>,
    ) -> Result<String, ErrorData> {
        let result = tools::health::handle(
            &self.workspaces.read().workspace,
            &self.db_cache,
            &self.config,
            &self.parse_coord,
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(result)
    }

    #[tool(description = "Agent-oriented help and recipes for sutra workflows. \
        Call with no args for a topic list. Call with topic (e.g. \"quickstart\", \
        \"review\", \"recipes\") for focused guidance with concrete tool invocation examples.")]
    pub async fn sutra_help(
        &self,
        Parameters(args): Parameters<HelpArgs>,
    ) -> Result<String, ErrorData> {
        let result = tools::help::handle(args.topic.as_deref()).map_err(sutra_to_rmcp)?;
        to_compact_json(result)
    }

    #[tool(description = "Project file skeleton ranked by importance. \
        Returns files sorted by (symbol_count + fan_in*2 + blast_radius).")]
    pub async fn sutra_map(
        &self,
        Parameters(args): Parameters<MapArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::map::handle_ctx(
            &ctx,
            args.path_prefix.as_deref(),
            args.limit,
            args.explain.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "File symbol table of contents — all symbols in a file with \
        qualified names, kinds, line ranges, and signatures. Default tier includes \
        signatures. Pass compact=true for structural fields only (no signatures); \
        pass verbose=true to add docstrings, complexity metrics, short_name, and \
        parent ids."
    )]
    pub async fn sutra_outline(
        &self,
        Parameters(args): Parameters<OutlineArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let detail = tools::outline::OutlineDetail::from_flags(args.compact, args.verbose);
        let result = tools::outline::handle(ctx.db(), &args.path, detail).map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "List discovered architectural components. Compact mode (default) returns name, file_count, top 3 anchors, and concept_density. Pass compact=false for full detail with UUIDs, complete file lists, and anchor rationale."
    )]
    pub async fn sutra_components(
        &self,
        Parameters(args): Parameters<ComponentsArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let compact = args.compact.unwrap_or(true);
        let result = tools::components::handle(ctx.db(), compact).map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "List discovered conventions. \
        Actions: list (all conventions).")]
    pub async fn sutra_conventions(
        &self,
        Parameters(args): Parameters<tools::conventions::ConventionsArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::conventions::handle(ctx.db(), &args).map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Manage architectural constraints and their violations. \
        Actions: list (all constraints with kind, severity, name, provenance, scope, waiver count), \
        violations (current constraint violations — covers forbidden_dep, \
        boundary, no_cycles, and the external-crate kinds forbidden_external / confined_external \
        [use-statement + Cargo manifest signals], and max_fan_in [fan-in threshold from DB rollups]), \
        waive (constraint_id or constraint_name, file_path, rationale, waived_by; optional \
        symbol_qualified_name), \
        unwaive (constraint_id or constraint_name, file_path; optional symbol_qualified_name). \
        Waivers and acks persist to version-controlled .sutra/accepted.toml, keyed by constraint \
        NAME; unwaive/unack remove by content key, not a row id."
    )]
    pub async fn sutra_constraints(
        &self,
        Parameters(args): Parameters<tools::constraints::ConstraintsArgs>,
    ) -> Result<String, ErrorData> {
        // tool_context first: it refreshes the index (query-path incremental
        // reparse, sutra/363) and releases the parse lock before returning.
        let ctx = self.tool_context(&args.workspace).await?;
        // Then hold the parse lock across evaluation so a reparse cannot remint
        // file ids mid-read and desync path_map from the engine's edges
        // (sutra/298). The refresh above already ran under this same lock, so
        // the DD read now reflects the post-refresh index.
        let _parse_guard = self.hold_parse_lock(&args.workspace).await?;
        let dd = self.get_dd_engine(&args.workspace);
        let result = tools::constraints::handle(ctx.db(), ctx.workspace_root(), Some(&dd), &args)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Look up symbols by NAME (functions, types, methods). \
        FTS5-backed over symbol names, signatures, and docstrings; the ONLY regex \
        operator honored is `|` for alternation (\"Foo|Bar\" matches either). \
        This is NOT a text search — for file text, comments, or string literals use rg; \
        for usages/call sites of a symbol use sutra_refs or sutra_calls; \
        for fuzzy or topic search use sutra_explore. \
        Returns compact results by default; pass detail=true for signatures and docstrings.")]
    pub async fn sutra_lookup(
        &self,
        Parameters(args): Parameters<LookupArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::lookup::handle_ctx(
            &ctx,
            &args.pattern,
            args.kind.as_deref(),
            args.limit,
            args.detail.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Explore a topic in the codebase. Resolves aliases (.sutra/aliases.toml), \
        qualified names (Foo::bar), and fuzzy queries. Returns a ranked list of matching \
        symbols — each with its signature and first doc line so you can pick the right one \
        without a follow-up fetch — plus literal sutra_symbol fetch instructions and a strategy \
        recommendation. Pass compact=true for the lean shape (no signature/doc). One call \
        replaces iterative map/outline/grep exploration."
    )]
    pub async fn sutra_explore(
        &self,
        Parameters(args): Parameters<ExploreArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        // Alias forward-resolution (the top tier below) reads the `aliases`
        // projection, which parse_workspace is the only other writer of. Gate it
        // at query time — like the accepted.toml cache — so alias edits take
        // effect regardless of transport, server CWD, or a frozen index that
        // never reparses. Cheap (hash of a tiny file) and a no-op when unchanged.
        if let Err(e) = crate::vocabulary::sync_aliases_if_changed(ctx.db(), ctx.workspace_root()) {
            tracing::warn!("alias sync during explore failed: {e}");
        }
        let budget = args.budget.unwrap_or(10);
        let compact = args.compact.unwrap_or(false);
        let result =
            tools::explore::handle(ctx.db(), ctx.workspace_root(), &args.query, budget, compact)
                .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Read ONE symbol's source code by name (a function/type/method), \
        not a file — to read a whole file use the built-in Read tool, or sutra_outline to list \
        its symbols first and then read a specific one. Includes 2 context lines around the \
        symbol by default (context_lines to change); returns a \
        stale warning if the file was deleted. Default 500-line cap; use full=true or limit=N \
        to override. Matching lessons are surfaced compactly (id + first sentence) — call \
        sutra_lessons(id=…) for the full text."
    )]
    pub async fn sutra_symbol(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::read::handle(
            ctx.db(),
            ctx.workspace_root(),
            &args.symbol,
            args.context_lines,
            args.limit,
            args.full.unwrap_or(false),
            ctx.is_stale(),
            args.imports.unwrap_or(true),
            Some(&self.lessons_db),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Token-budgeted context packing for a symbol. \
        Packs the target symbol + dependencies + dependents within a token budget, \
        with graceful degradation: full body → head-truncated → signature → omitted. \
        Priority cascade: target > direct deps > direct dependents > transitive deps > \
        transitive dependents. Neighbors sorted by edge weight (call > field_access > \
        type_use > import > reference) then pagerank. Tests tallied, not packed. \
        Returns context array with role/content/tokens per entry, plus omitted counts.")]
    pub async fn sutra_context(
        &self,
        Parameters(args): Parameters<ContextArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::context::handle(
            ctx.db(),
            ctx.workspace_root(),
            &args.symbol,
            args.token_budget,
            args.depth,
            ctx.is_stale(),
            Some(&self.lessons_db),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Store or cite a code-anchored lesson. \
        Store mode: provide text + location_anchors, each either a bare string \
        (symbol name or file path — kind is inferred) or {kind, value} where kind \
        is symbol|file|import_pattern|directory; \
        pass workspace to auto-enrich with import-pattern anchors, directory anchors, \
        and language/technology categories from the workspace graph. \
        A category naming a language scopes the lesson to workspaces in that language \
        (shorthands like py/ts/golang are recognised; prefix anything else with lang:). \
        Cite mode: provide cite=<lesson_id> to record a citation and increase confidence; \
        when confidence reaches the threshold the lesson becomes verified. \
        Anti-verify: provide cite + anti_verify=true to flag a lesson as wrong (decreases confidence).")]
    pub async fn sutra_remember(
        &self,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<String, ErrorData> {
        let ws_db = args
            .workspace
            .as_deref()
            .and_then(|ws| self.get_db(ws).ok());
        let result = tools::remember::handle(&self.lessons_db, ws_db.as_deref(), &args)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(result)
    }

    #[tool(
        description = "Search stored lessons by text (FTS5) and/or structured filters \
        (category, symbol anchor, verified). A query also matches category tags, so \
        intent-shaped lookups work before the code exists — sutra_lessons(query=\"sqlite \
        migration\") finds lessons tagged `sqlite` even when they never say the word. \
        Text hits rank first; each result carries the match_kind that produced it. \
        Pass id=… to fetch the full text of a lesson surfaced compactly by sutra_symbol."
    )]
    pub async fn sutra_lessons(
        &self,
        Parameters(args): Parameters<tools::lessons::LessonsArgs>,
    ) -> Result<String, ErrorData> {
        let result = tools::lessons::handle(&self.lessons_db, &args).map_err(sutra_to_rmcp)?;
        to_compact_json(result)
    }

    #[tool(
        description = "Blast radius analysis for a symbol. Counts direct callers, \
        runs transitive BFS (depth 3), and computes risk level (low/medium/high). \
        Also acknowledges every file that defines the symbol for the modification \
        guard, clearing its load-bearing block on a retried edit; the acked paths \
        are returned in `guard_ack`."
    )]
    pub async fn sutra_impact(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let mut result = tools::impact::handle(
            ctx.db(),
            &args.symbol,
            args.explain.unwrap_or(false),
            Some(&self.lessons_db),
            ctx.workspace_root(),
        )
        .map_err(sutra_to_rmcp)?;
        // Acknowledge the file(s) for the modification guard. The guard blocks a
        // load-bearing *file* and names one of its hot symbols; the agent runs
        // sutra_impact on that name, which resolves to one arbitrary definition
        // site. When the entity spans several files (a struct + its impl blocks
        // all share a qualified name), that site need not be the file being
        // edited, so a single-file ack silently fails to clear the guard
        // (sutra/329). Ack every definition site so the retry always lands, and
        // report the set so the caller can see the handshake completed.
        let mut acked: Vec<String> = Vec::new();
        if let Some(qname) = result["symbol"].as_str()
            && let Ok(files) = ctx.db().symbol_definition_files(qname)
        {
            acked = files;
        }
        if let Some(file_path) = result["file"].as_str()
            && !acked.iter().any(|p| p == file_path)
        {
            acked.push(file_path.to_string());
        }
        for path in &acked {
            guard::touch_ack(ctx.workspace_root(), path);
        }
        result["guard_ack"] = serde_json::json!(acked);
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "File dependency graph from import edges. \
        If path given, BFS from that file to depth. \
        If cycles=true, detect import cycle groups (SCCs) instead of listing edges.")]
    pub async fn sutra_deps(
        &self,
        Parameters(args): Parameters<DepsArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let cycles = args.cycles.unwrap_or(false);
        let result = tools::deps::handle(ctx.db(), args.path.as_deref(), args.depth, cycles)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "All usages of a symbol across the codebase. \
        Groups references by file with line numbers. Optional context_kind filter \
        (e.g. \"construction\", \"call\", \"type_use\") to narrow results.")]
    pub async fn sutra_refs(
        &self,
        Parameters(args): Parameters<RefsArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::refs::handle(ctx.db(), &args.symbol, args.context_kind.as_deref())
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Call hierarchy for a function. \
        direction=callers (default) or callees. BFS to depth (default 1, max 3).")]
    pub async fn sutra_calls(
        &self,
        Parameters(args): Parameters<CallsArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::calls::handle(
            ctx.db(),
            &args.symbol,
            args.direction.as_deref(),
            args.depth,
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Trace call chains through the codebase. \
        direction=forward (default): finds paths from entry points to the symbol. \
        direction=backward: finds paths from the symbol to leaf functions. \
        Detects and marks cycles. Entry points: main, Dart lifecycle methods, \
        or any symbol with zero callers.")]
    pub async fn sutra_trace(
        &self,
        Parameters(args): Parameters<TraceArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::trace::handle(
            ctx.db(),
            &args.symbol,
            args.direction.as_deref(),
            args.limit,
            args.follow_fields,
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Blast radius of a git diff. \
        Shows changed files, affected symbols, and their callers.")]
    pub async fn sutra_diff_impact(
        &self,
        Parameters(args): Parameters<DiffImpactArgs>,
    ) -> Result<String, ErrorData> {
        self.await_parse(&args.workspace).await;
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::diff_impact::handle(
            ctx.db(),
            ctx.workspace_root(),
            args.base.as_deref(),
            args.head.as_deref(),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Per-commit structural manifest for a commit range. \
        Returns each commit with its changed files and symbol-level change \
        classifications (added/deleted/signature_changed/body_changed). \
        Use for multi-commit branch review where per-commit intent matters. \
        Defaults to branch range (merge-base..HEAD). Max 50 commits.")]
    pub async fn sutra_commit_manifest(
        &self,
        Parameters(args): Parameters<CommitManifestArgs>,
    ) -> Result<String, ErrorData> {
        self.await_parse(&args.workspace).await;
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::commit_manifest::handle(
            ctx.db(),
            ctx.workspace_root(),
            args.base.as_deref(),
            args.head.as_deref(),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Composite PR risk score (0.0–1.0) for a git diff. \
        Combines blast_radius, complexity, churn, and volume signals with \
        documented weights. Returns per-signal breakdown and top-N riskiest \
        changed symbols.")]
    pub async fn sutra_pr_risk(
        &self,
        Parameters(args): Parameters<PrRiskArgs>,
    ) -> Result<String, ErrorData> {
        self.await_parse(&args.workspace).await;
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::pr_risk::handle(
            ctx.db(),
            ctx.workspace_root(),
            args.base.as_deref(),
            args.head.as_deref(),
            args.explain.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Git history of a symbol's file with commit classification \
        (feature, bugfix, refactor, test, docs, chore, performance, unknown). \
        Uses --follow for rename tracking."
    )]
    pub async fn sutra_provenance(
        &self,
        Parameters(args): Parameters<ProvenanceArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::provenance::handle(ctx.db(), ctx.workspace_root(), &args.symbol)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Entities that historically change together in git history. \
        granularity='file' (default): file-level co-change by path. \
        granularity='function': function-level co-change by qualified symbol name. \
        Reports both jaccard and confidence metrics for function granularity."
    )]
    pub async fn sutra_cochange(
        &self,
        Parameters(args): Parameters<CochangeArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::cochange::handle(
            ctx.db(),
            &args.path,
            args.threshold,
            args.granularity.as_deref(),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Structural review compositor. Diffs current branch (or staged/unstaged), \
        identifies changed files and symbols, computes transitive impact, calculates a \
        0.0–1.0 risk score with breakdown, and ranks recommended reads. \
        health_delta separates a temporal comparison of persistent health (measured only \
        between complete observations under the same scoring basis, else incomparable with \
        a reason) from on_demand attribution of fresh blame/shape findings against the \
        current evidence. \
        diff: \"branch\" (default, against main merge-base), \"staged\", \"unstaged\", \
        or a commit spec — \"abc123..def456\" for a range, \"abc123\" for a single commit."
    )]
    pub async fn sutra_review(
        &self,
        Parameters(args): Parameters<ReviewArgs>,
    ) -> Result<String, ErrorData> {
        // Pin the baseline snapshot BEFORE tool_context can full-parse (stamp
        // heal) and record a newer checkpoint — otherwise the health delta would
        // compare current against a snapshot written by THIS request and hide real
        // debt (sutra/415 contract). A read-only peek at the latest checkpoint id.
        // A genuinely-missing baseline (no checkpoint at pin time) is pinned as
        // `Pinned(None)` and must stay missing → incomparable (sutra/424 F5), not
        // healed into the fresh snapshot the reparse below may write.
        let baseline = crate::health::compare::BaselineSelector::Pinned(
            self.get_db(&args.workspace)
                .ok()
                .and_then(|db| db.latest_snapshots(1).ok())
                .and_then(|snaps| snaps.first().map(|s| s.id)),
        );

        // tool_context first: it refreshes the index (query-path incremental
        // reparse, sutra/363) and releases the parse lock before returning.
        let ctx = self.tool_context(&args.workspace).await?;
        // Then hold (not merely await) the parse lock across the DD-backed
        // review so a reparse cannot remint file ids mid-evaluation (sutra/298).
        // This subsumes the previous await_parse, which only waited for an
        // in-flight parse and then released before evaluation ran.
        let _parse_guard = self.hold_parse_lock(&args.workspace).await?;
        // Refresh persistent health within the held coordinator lock so the delta's
        // persistent side reflects the current index (an incremental reparse above
        // does not recompute health). Uses the locked core, not the acquiring
        // adapter, to avoid re-locking the coordinator. Best-effort.
        // Capture the refresh outcome: a Deferred/Failed/stale refresh leaves the
        // current run unverified, so its outcomes read as Missing — the temporal
        // side is partial (incomparable) and attribution only conditional (sutra/416).
        let refresh_outcome =
            self.refresh_health_locked(ctx.db(), ctx.workspace_root(), &args.workspace);
        let dd = self.get_dd_engine(&args.workspace);
        let result = tools::review::handle(
            ctx.db(),
            ctx.workspace_root(),
            args.diff.as_deref(),
            Some(&dd),
            baseline,
            refresh_outcome,
            args.explain.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Find dead symbols (zero inbound references) and unreachable files \
        (zero importers). Automatically excludes #[test]/#[bench] functions, items inside \
        #[cfg(test)] modules, #[no_mangle]/FFI entrypoints, and integration test files."
    )]
    pub async fn sutra_dead(
        &self,
        Parameters(args): Parameters<DeadArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::dead::handle(
            ctx.db(),
            args.path_prefix.as_deref(),
            args.include_pub.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Riskiest files ranked by git churn × blast radius × complexity.")]
    pub async fn sutra_hotspots(
        &self,
        Parameters(args): Parameters<HotspotsArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::hotspots::handle_ctx(&ctx, args.window_days, args.limit)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Per-file and per-component health report. Returns derived health scores \
        (1.0-10.0), active findings with full detail, category deductions, and component \
        instability (Martin's Ce/(Ca+Ce)). A file or component with missing analysis is \
        partial: health_score is null and score_bounds gives {lower, upper}, with the \
        missing producers and reasons. Filter by file path or component name. \
        Default mode='actionable' shows only files with findings; mode='all' includes everything. \
        Worst files first."
    )]
    pub async fn sutra_file_health(
        &self,
        Parameters(args): Parameters<FileHealthArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        // Demand-refresh persistent health evidence, then score the coherent run
        // (sutra/415 Wave D, sutra/416): the refresh outcome is the run's validity —
        // a deferred/failed refresh makes its outcomes Missing, so scores are bounds.
        let refresh_outcome = self.refresh_health(&args.workspace).await;
        let mut result = tools::file_health::handle_ctx(
            &ctx,
            refresh_outcome,
            args.path.as_deref(),
            args.limit,
            args.mode.as_deref(),
            args.component.as_deref(),
            args.explain.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        tools::file_health::attach_health_evidence(ctx.db(), &mut result, refresh_outcome)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Compare two parse snapshots with per-file and per-component health deltas, \
        or query a single file's health history over time. \
        Defaults to comparing the two most recent snapshots. \
        Set 'path' to get a per-file time series instead of a comparison. \
        Only complete-vs-complete pairs scored under the same basis (waivers, weights, \
        versions, applicability) are measured improved/degraded; partial, legacy, \
        basis-changed, new and removed files are 'incomparable' with a reason. \
        Component deltas are likewise measured only under a matching membership basis. \
        Workspace/category health deltas are null unless aggregate_comparison.measured."
    )]
    pub async fn sutra_trend(
        &self,
        Parameters(args): Parameters<TrendArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::trend::handle(ctx.db(), &args).map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Multi-axis composite query. AND-intersects filters (kind, \
        min_complexity, min_churn, calls_to, file_glob, name_regex) and ranks results \
        by importance (PageRank), complexity, or churn. Each result includes per-axis \
        values."
    )]
    pub async fn sutra_winnow(
        &self,
        Parameters(args): Parameters<WinnowArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let filter = tools::winnow::WinnowFilter {
            kind: args.kind,
            min_complexity: args.min_complexity,
            min_churn: args.min_churn,
            churn_window_days: args.churn_window_days,
            calls_to: args.calls_to,
            file_glob: args.file_glob,
            name_regex: args.name_regex,
            rank_by: args.rank_by,
            limit: args.limit,
        };
        let result = tools::winnow::handle(ctx.db(), ctx.workspace_root(), &filter)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Find structurally similar functions using HRR vector similarity. \
            With symbol: finds functions similar to it. Mode 'strip' (default) matches AST shape \
            regardless of identifiers; mode 'embed' matches structure and naming. \
            Without symbol: finds all near-duplicate pattern families (groups of 3+ functions \
            with near-identical AST structure)."
    )]
    pub async fn sutra_similar(
        &self,
        Parameters(args): Parameters<SimilarArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace).await?;
        let result = tools::similar::handle(
            ctx.db(),
            args.symbol.as_deref(),
            args.mode.as_deref(),
            args.limit,
            args.threshold,
            args.min_group,
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Workspace lifecycle. \
            Actions: 'status' (default — register workspace, return health/counts/freshness), \
            'reparse' (register + synchronous reparse).")]
    pub async fn sutra_workspace(
        &self,
        Parameters(args): Parameters<WorkspaceToolArgs>,
    ) -> Result<String, ErrorData> {
        let path = args.path.as_deref().ok_or_else(|| {
            ErrorData::new(
                rmcp::model::ErrorCode(crate::error::codes::INVALID_PARAMS),
                "sutra_workspace requires a `path` (absolute path to the project root)."
                    .to_string(),
                None,
            )
        })?;
        let action = args.action.as_deref().unwrap_or("status");

        let (ws_id, entry, _) = self.register_workspace(path, args.languages)?;
        let entry = Arc::new(entry);
        let db = self.get_db(&ws_id)?;

        match action {
            "status" => {
                let needs_parse = db.last_parse_time().ok().flatten().is_none();
                if needs_parse {
                    // Hold the parse lock across the first parse so it excludes a
                    // concurrent DD-backed evaluation the same way the reparse
                    // action does — otherwise the never-parsed path could remint
                    // ids under an in-flight constraints/orient read (sutra/298).
                    let lock = self.parse_coord.lock_for(&ws_id);
                    let guard = lock.lock_owned().await;
                    let mark = self.parse_coord.mark_parsing(&ws_id);
                    let entry_bg = Arc::clone(&entry);
                    let db_bg = Arc::clone(&db);
                    let config_bg = Arc::clone(&self.config);
                    // Guard + mark move into the blocking task so a cancelled
                    // request future can't release the parse lock while
                    // `parse_workspace` is still writing (sutra/380).
                    let _ = tokio::task::spawn_blocking(move || {
                        let _guard = guard;
                        let _mark = mark;
                        let cancel = AtomicBool::new(false);
                        let registry = crate::parser::adapter::default_registry();
                        crate::pipeline::parse_workspace(
                            &entry_bg, &db_bg, &config_bg, &cancel, &registry,
                        )
                    })
                    .await;
                }

                let files = db.all_files().unwrap_or_default();
                let sym_counts = db.symbol_counts_by_file().unwrap_or_default();
                let total_symbols: i64 = sym_counts.values().sum();
                let freshness = self.freshness(&db, &entry.root);

                let status = if files.is_empty() { "empty" } else { "ready" };

                let mut val = serde_json::json!({
                    "workspace": ws_id,
                    "root": entry.root.display().to_string(),
                    "status": status,
                    "last_parse": freshness["as_of"],
                    "is_stale": freshness["is_stale"],
                    "files": files.len(),
                    "symbols": total_symbols,
                });
                if freshness.get("parsing_in_progress") == Some(&serde_json::Value::Bool(true)) {
                    val["parsing_in_progress"] = serde_json::Value::Bool(true);
                }
                if files.is_empty()
                    && entry.root.is_dir()
                    && std::fs::read_dir(&entry.root).is_ok_and(|mut d| d.next().is_some())
                {
                    val["warnings"] = serde_json::json!([
                        "workspace root exists but 0 files indexed — check languages config matches adapter IDs (rust, dart)"
                    ]);
                }
                to_compact_json(val)
            }
            "reparse" => {
                let ws_root = entry.root.as_path().to_owned();
                let lock = self.parse_coord.lock_for(&ws_id);
                let guard = lock.lock_owned().await;
                let mark = self.parse_coord.mark_parsing(&ws_id);
                let config = Arc::clone(&self.config);
                let db_bg = Arc::clone(&db);
                // Guard + mark move into the blocking task so a cancelled request
                // future can't release the parse lock while the reparse is still
                // writing (sutra/380).
                let result = tokio::task::spawn_blocking(move || {
                    let _guard = guard;
                    let _mark = mark;
                    let cancel = AtomicBool::new(false);
                    let registry = crate::parser::adapter::default_registry();
                    tools::parse::handle(&entry, &db_bg, &config, &cancel, &registry)
                })
                .await
                .map_err(|e| {
                    ErrorData::new(
                        rmcp::model::ErrorCode(crate::error::codes::INTERNAL_ERROR),
                        format!("parse task panicked: {e}"),
                        None,
                    )
                })?
                .map_err(sutra_to_rmcp)?;
                self.wrap_response(&db, &ws_root, result)
            }
            other => Err(ErrorData::new(
                rmcp::model::ErrorCode(crate::error::codes::INVALID_PARAMS),
                format!(
                    "unknown action: \"{other}\". Valid actions: \"status\" (default), \"reparse\"."
                ),
                None,
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SutraServer {
    fn get_info(&self) -> ServerInfo {
        // This string is the one piece of sutra prose that survives MCP tool
        // deferral: when a host withholds tool schemas and sends bare names, the
        // `instructions` field still lands whole. So it carries a batch-load line
        // (defuse the deferral tax) and a decision rule per core tool — not a
        // roster of names the agent already has. Keep it under ~1000 chars; the
        // full reference is sutra_help(). See sutra/367.
        let instructions = format!(
            "sutra v{version} — code intelligence for manas: a symbol graph — callers, callees, \
             file:line spans. Prefer it over grep/read.\n\n\
             If deferred (names shown, schemas withheld), load the core set in ONE lookup, \
             never one at a time: ToolSearch \"select:mcp__sutra__sutra_explore,\
             mcp__sutra__sutra_symbol,mcp__sutra__sutra_outline,mcp__sutra__sutra_refs,\
             mcp__sutra__sutra_map,mcp__sutra__sutra_impact\".\n\n\
             - sutra_explore: start here for any symbol/topic; resolves .sutra/aliases.toml \
             terms first, then ranks hits with a strategy hint (sutra_lookup for an exact name; \
             rg for file text).\n\
             - sutra_symbol: read one symbol's source by name, not a whole file.\n\
             - sutra_outline: a file's symbol table of contents.\n\
             - sutra_refs: every usage or call site, before a rename.\n\
             - sutra_map: discover files instead of find/ls.\n\
             - sutra_impact: blast radius before editing a hot file.\n\n\
             Edits pass a house-rules guard. Responses carry as_of + is_stale; \
             the graph refreshes before each query. sutra_help() is the long-form reference.",
            version = env!("CARGO_PKG_VERSION"),
        );
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions(instructions)
    }
}

// ---------------------------------------------------------------------------
// Response serialization
// ---------------------------------------------------------------------------

fn to_compact_json(val: serde_json::Value) -> Result<String, ErrorData> {
    serde_json::to_string(&val).map_err(json_to_rmcp)
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn sutra_to_rmcp(e: SutraError) -> ErrorData {
    let data = e.data();
    ErrorData::new(
        rmcp::model::ErrorCode(e.code()),
        e.message(),
        Some(serde_json::to_value(data).unwrap_or_default()),
    )
}

fn json_to_rmcp(e: serde_json::Error) -> ErrorData {
    let data = crate::error::ErrorData {
        tool: "unknown",
        argument: None,
        constraint: "response must serialize to JSON".to_string(),
        received: None,
        next_action: "This is an internal error. Retry or report the issue.".to_string(),
    };
    ErrorData::new(
        rmcp::model::ErrorCode::INTERNAL_ERROR,
        format!("JSON serialization failed: {e}"),
        Some(serde_json::to_value(data).unwrap_or_default()),
    )
}

#[cfg(test)]
mod refresh_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn test_config(db_dir: &Path) -> Config {
        Config {
            db_dir: db_dir.to_path_buf(),
            workspaces_path: db_dir.join("workspaces.toml"),
            listen_addr: "127.0.0.1:0".to_string(),
            parse_parallelism: 1,
            log_level: "warn".to_string(),
            constraints_idle_timeout_sec: 1800,
            parse_timeout_ms: 5000,
        }
    }

    // sutra/382: a clean-byte parser-stamp mismatch must heal on the query path
    // (a full re-extraction), not only at stdio-CWD startup. The query path is
    // the single choke point http and non-CWD workspaces reach. The server here
    // has default_workspace = None (the http configuration) and the workspace is
    // passed explicitly — the non-CWD case that maybe_reparse_cwd never covers.
    #[tokio::test]
    async fn test_query_path_heals_parser_stamp_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "pub fn alpha() {}\n").unwrap();

        let db_dir = tempfile::tempdir().unwrap();
        let config = Arc::new(test_config(db_dir.path()));
        let entry = workspace::WorkspaceEntry {
            id: "stamp-heal".to_string(),
            root: dir.path().to_path_buf(),
            languages: vec!["rust".to_string()],
            frozen: false,
        };
        let db = Arc::new(Db::open_unchecked(&entry.id, db_dir.path()).unwrap());

        // Baseline full parse records the current extractor stamp.
        {
            let cancel = AtomicBool::new(false);
            let registry = crate::parser::adapter::default_registry();
            crate::pipeline::parse_workspace(&entry, &db, &config, &cancel, &registry).unwrap();
        }
        assert_eq!(
            db.parser_stamp().unwrap().as_deref(),
            Some(crate::parser::PARSER_STAMP),
            "baseline parse records the current stamp",
        );

        // Simulate an extractor change: the stored stamp no longer matches, but
        // the bytes on disk are untouched — there is no content drift to ride.
        db.set_parser_stamp("stale-extractor-identity").unwrap();

        let server = SutraServer::new(
            Arc::clone(&config),
            Arc::new(RwLock::new(WorkspacesConfig {
                workspace: vec![entry.clone()],
            })),
            Arc::new(Mutex::new(HashMap::new())),
            ParseCoordinator::new(),
            Arc::new(LessonsDb::open(db_dir.path()).unwrap()),
        );

        // The query-path refresh sees the stamp mismatch (not content drift) and
        // runs a full re-extraction, which re-records the current stamp. Only
        // parse_workspace touches the stamp — parse_incremental never does — so
        // the stamp advancing is proof the full path ran.
        let note = server.refresh_before_answer(&db, entry.clone()).await;
        assert!(
            note.is_none(),
            "a successful heal returns no degradation note: {note:?}",
        );
        assert_eq!(
            db.parser_stamp().unwrap().as_deref(),
            Some(crate::parser::PARSER_STAMP),
            "the query-path refresh heals a clean-byte stamp mismatch via a full reparse",
        );

        // Healed: a second refresh sees a matching stamp and clean bytes → no-op.
        let note2 = server.refresh_before_answer(&db, entry).await;
        assert!(note2.is_none(), "no reparse once healed");
    }
}
