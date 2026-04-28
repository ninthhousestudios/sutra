use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use parking_lot::Mutex;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::Config;
use crate::db::Db;
use crate::error::SutraError;
use crate::tools;
use crate::workspace::{self, WorkspacesConfig};

// ---------------------------------------------------------------------------
// Args structs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EmptyArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WorkspaceArgs {
    pub workspace: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MapArgs {
    pub workspace: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OutlineArgs {
    pub workspace: String,
    pub path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FindArgs {
    pub workspace: String,
    pub name: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepArgs {
    pub workspace: String,
    pub pattern: String,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    pub workspace: String,
    pub symbol: String,
    #[serde(default)]
    pub context_lines: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ImpactArgs {
    pub workspace: String,
    pub symbol: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DepsArgs {
    pub workspace: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub depth: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolsMetaArgs {
    #[serde(default)]
    pub enable: Option<Vec<String>>,
    #[serde(default)]
    pub disable: Option<Vec<String>>,
    #[serde(default)]
    pub list: Option<bool>,
}

// ---------------------------------------------------------------------------
// SutraServer
// ---------------------------------------------------------------------------

pub struct SutraServer {
    db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
    config: Arc<Config>,
    workspaces: Arc<WorkspacesConfig>,
    analysis_enabled: Arc<AtomicBool>,
    tool_router: ToolRouter<Self>,
}

impl Clone for SutraServer {
    fn clone(&self) -> Self {
        Self {
            db_cache: Arc::clone(&self.db_cache),
            config: Arc::clone(&self.config),
            workspaces: Arc::clone(&self.workspaces),
            analysis_enabled: Arc::clone(&self.analysis_enabled),
            tool_router: Self::tool_router(),
        }
    }
}

impl SutraServer {
    pub fn new(
        config: Arc<Config>,
        workspaces: Arc<WorkspacesConfig>,
        db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
    ) -> Self {
        Self {
            db_cache,
            config,
            workspaces,
            analysis_enabled: Arc::new(AtomicBool::new(false)),
            tool_router: Self::tool_router(),
        }
    }

    fn resolve_workspace(
        &self,
        ws_id: &str,
    ) -> std::result::Result<crate::workspace::WorkspaceEntry, ErrorData> {
        workspace::resolve_workspace(&self.workspaces, ws_id)
            .cloned()
            .map_err(sutra_to_rmcp)
    }

    fn get_db(&self, ws_id: &str) -> std::result::Result<Arc<Db>, ErrorData> {
        tools::get_or_open_db(&self.db_cache, ws_id, &self.config.db_dir).map_err(sutra_to_rmcp)
    }

    fn freshness(&self, db: &Db) -> serde_json::Value {
        let (as_of, is_stale) = match db.last_parse_time() {
            Ok(Some(ts)) => {
                let is_stale = chrono::DateTime::parse_from_rfc3339(&ts)
                    .map(|dt| {
                        let age = chrono::Utc::now() - dt.with_timezone(&chrono::Utc);
                        age.num_seconds() as u64 > self.config.stale_threshold_sec
                    })
                    .unwrap_or(true);
                (Some(ts), is_stale)
            }
            _ => (None, true),
        };
        serde_json::json!({ "as_of": as_of, "is_stale": is_stale })
    }

    fn wrap_response(
        &self,
        db: &Db,
        mut result: serde_json::Value,
    ) -> std::result::Result<String, ErrorData> {
        if let Some(obj) = result.as_object_mut() {
            let f = self.freshness(db);
            if let Some(f_obj) = f.as_object() {
                for (k, v) in f_obj {
                    obj.insert(k.clone(), v.clone());
                }
            }
        }
        serde_json::to_string_pretty(&result).map_err(json_to_rmcp)
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
        let result =
            tools::health::handle(&self.workspaces.workspace, &self.db_cache, &self.config)
                .map_err(sutra_to_rmcp)?;
        serde_json::to_string_pretty(&result).map_err(json_to_rmcp)
    }

    #[tool(description = "Project file skeleton ranked by importance. \
        Returns files sorted by (symbol_count + fan_in*2 + blast_radius).")]
    pub async fn sutra_map(
        &self,
        Parameters(args): Parameters<MapArgs>,
    ) -> Result<String, ErrorData> {
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result =
            tools::map::handle(&db, args.path_prefix.as_deref(), args.limit).map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "File symbol table of contents — all symbols in a file with \
        qualified names, kinds, line ranges, and signatures.")]
    pub async fn sutra_outline(
        &self,
        Parameters(args): Parameters<OutlineArgs>,
    ) -> Result<String, ErrorData> {
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::outline::handle(&db, &args.path).map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Jump to a symbol definition by name. Three-tier search: \
        exact short_name, exact qualified_name, then FTS5 fuzzy.")]
    pub async fn sutra_find(
        &self,
        Parameters(args): Parameters<FindArgs>,
    ) -> Result<String, ErrorData> {
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::find::handle(&db, &args.name, args.kind.as_deref(), args.limit)
            .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Search indexed symbols by name pattern. \
        FTS5-backed search across symbol names, signatures, and docstrings.")]
    pub async fn sutra_grep(
        &self,
        Parameters(args): Parameters<GrepArgs>,
    ) -> Result<String, ErrorData> {
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::grep::handle(&db, &args.pattern, args.kind.as_deref(), args.limit)
            .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Read a symbol's source code from disk with line numbers. \
        Includes context lines around the symbol. Returns stale warning if file was deleted.")]
    pub async fn sutra_read(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<String, ErrorData> {
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result =
            tools::read::handle(&db, &ws.root, &args.symbol, args.context_lines).map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Blast radius analysis for a symbol. Counts direct callers, \
        runs transitive BFS (depth 3), and computes risk level (low/medium/high).")]
    pub async fn sutra_impact(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<String, ErrorData> {
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::impact::handle(&db, &args.symbol).map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "File dependency graph from import edges. \
        If path given, BFS from that file to depth. Otherwise returns all edges.")]
    pub async fn sutra_deps(
        &self,
        Parameters(args): Parameters<DepsArgs>,
    ) -> Result<String, ErrorData> {
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::deps::handle(&db, args.path.as_deref(), args.depth).map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Trigger a workspace reparse. Use after editing files to get \
        fresh results from other tools.")]
    pub async fn sutra_parse(
        &self,
        Parameters(args): Parameters<WorkspaceArgs>,
    ) -> Result<String, ErrorData> {
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::parse::handle(&ws, &db, &self.config)
            .await
            .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Manage tool tiers. Enable or disable the analysis tier \
        (sutra_refs, sutra_calls, sutra_diff_impact, sutra_cochange). \
        Use list=true to see available tiers and their status.")]
    pub async fn sutra_tools(
        &self,
        Parameters(args): Parameters<ToolsMetaArgs>,
    ) -> Result<String, ErrorData> {
        let result = tools::tools_meta::handle(
            &self.analysis_enabled,
            args.enable.as_deref(),
            args.disable.as_deref(),
            args.list.unwrap_or(false),
        );
        serde_json::to_string_pretty(&result).map_err(json_to_rmcp)
    }
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SutraServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "sutra v0.1.0 — code intelligence for manas. \
             Core tools: sutra_health, sutra_map, sutra_outline, sutra_find, \
             sutra_grep, sutra_read, sutra_impact, sutra_deps, sutra_parse, sutra_tools. \
             Analysis tools (enable via sutra_tools): sutra_refs, sutra_calls, \
             sutra_diff_impact, sutra_cochange. \
             All responses include as_of timestamp and is_stale indicator.",
        )
    }
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
