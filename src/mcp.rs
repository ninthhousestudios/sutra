use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use parking_lot::{Mutex, RwLock};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerInfo};
use rmcp::{ErrorData, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::config::Config;
use crate::db::Db;
use crate::error::SutraError;
use crate::guard;
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RefsArgs {
    pub workspace: String,
    pub symbol: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CallsArgs {
    pub workspace: String,
    pub symbol: String,
    #[serde(default)]
    pub direction: Option<String>,
    #[serde(default)]
    pub depth: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiffImpactArgs {
    pub workspace: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CochangeArgs {
    pub workspace: String,
    pub path: String,
    #[serde(default)]
    pub window_days: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraceArgs {
    pub workspace: String,
    pub symbol: String,
    /// "forward" (entry points → symbol, default) or "backward" (symbol → leaves)
    #[serde(default)]
    pub direction: Option<String>,
    /// Max number of paths to return (default 10)
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PrRiskArgs {
    pub workspace: String,
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub head: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ProvenanceArgs {
    pub workspace: String,
    pub symbol: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeadArgs {
    pub workspace: String,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default)]
    pub include_pub: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HotspotsArgs {
    pub workspace: String,
    #[serde(default)]
    pub window_days: Option<u32>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct FileHealthArgs {
    pub workspace: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AddRootArgs {
    /// Absolute path to the workspace root directory
    pub path: String,
    /// Languages to index (default: ["rust", "dart"])
    #[serde(default)]
    pub languages: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StatusArgs {
    /// Absolute path to the workspace root directory
    pub path: String,
    /// Languages to index (default: ["rust", "dart"])
    #[serde(default)]
    pub languages: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TrendArgs {
    pub workspace: String,
    /// ISO timestamp for the start of the comparison window.
    /// Defaults to the second-most-recent snapshot.
    #[serde(default)]
    pub from: Option<String>,
    /// ISO timestamp for the end of the comparison window.
    /// Defaults to the most recent snapshot.
    #[serde(default)]
    pub to: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WinnowArgs {
    pub workspace: String,
    /// Filter by symbol kind (function, method, struct, etc.)
    #[serde(default)]
    pub kind: Option<String>,
    /// Minimum cognitive complexity
    #[serde(default)]
    pub min_complexity: Option<i64>,
    /// Minimum git churn (commit count in window)
    #[serde(default)]
    pub min_churn: Option<u32>,
    /// Churn window in days (default 90)
    #[serde(default)]
    pub churn_window_days: Option<u32>,
    /// Symbol name that results must call
    #[serde(default)]
    pub calls_to: Option<String>,
    /// Glob pattern for file paths
    #[serde(default)]
    pub file_glob: Option<String>,
    /// Regex for symbol names (matched against qualified_name and short_name)
    #[serde(default)]
    pub name_regex: Option<String>,
    /// Rank results by: "importance" (default), "complexity", "churn"
    #[serde(default)]
    pub rank_by: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

// ---------------------------------------------------------------------------
// SutraServer
// ---------------------------------------------------------------------------

pub struct SutraServer {
    db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
    config: Arc<Config>,
    workspaces: Arc<RwLock<WorkspacesConfig>>,
    analysis_enabled: Arc<AtomicBool>,
    parsing_in_progress: Arc<Mutex<HashSet<String>>>,
    tool_router: ToolRouter<Self>,
}

impl Clone for SutraServer {
    fn clone(&self) -> Self {
        Self {
            db_cache: Arc::clone(&self.db_cache),
            config: Arc::clone(&self.config),
            workspaces: Arc::clone(&self.workspaces),
            analysis_enabled: Arc::clone(&self.analysis_enabled),
            parsing_in_progress: Arc::clone(&self.parsing_in_progress),
            tool_router: Self::tool_router(),
        }
    }
}

impl SutraServer {
    pub fn new(
        config: Arc<Config>,
        workspaces: Arc<RwLock<WorkspacesConfig>>,
        db_cache: Arc<Mutex<HashMap<String, Arc<Db>>>>,
    ) -> Self {
        Self {
            db_cache,
            config,
            workspaces,
            analysis_enabled: Arc::new(AtomicBool::new(false)),
            parsing_in_progress: Arc::new(Mutex::new(HashSet::new())),
            tool_router: Self::tool_router(),
        }
    }

    fn resolve_workspace(
        &self,
        ws_id: &str,
    ) -> std::result::Result<crate::workspace::WorkspaceEntry, ErrorData> {
        workspace::resolve_workspace(&self.workspaces.read(), ws_id)
            .cloned()
            .map_err(sutra_to_rmcp)
    }

    fn get_db(&self, ws_id: &str) -> std::result::Result<Arc<Db>, ErrorData> {
        tools::get_or_open_db(&self.db_cache, ws_id, &self.config.db_dir).map_err(sutra_to_rmcp)
    }

    fn require_analysis(&self) -> std::result::Result<(), ErrorData> {
        if !self.analysis_enabled.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ErrorData::new(
                rmcp::model::ErrorCode(crate::error::codes::INVALID_PARAMS),
                "Analysis tier not enabled. Call sutra_tools with enable: [\"analysis\"] first."
                    .to_string(),
                None,
            ));
        }
        Ok(())
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
            tools::health::handle(&self.workspaces.read().workspace, &self.db_cache, &self.config)
                .map_err(sutra_to_rmcp)?;
        serde_json::to_string_pretty(&result).map_err(json_to_rmcp)
    }

    #[tool(description = "Project file skeleton ranked by importance. \
        Returns files sorted by (symbol_count + fan_in*2 + blast_radius).")]
    pub async fn sutra_map(
        &self,
        Parameters(args): Parameters<MapArgs>,
    ) -> Result<String, ErrorData> {
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::map::handle_with_freshness(
            &db,
            args.path_prefix.as_deref(),
            args.limit,
            Some(ws.root.as_path()),
        )
        .map_err(sutra_to_rmcp)?;
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
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::find::handle_with_freshness(
            &db,
            &args.name,
            args.kind.as_deref(),
            args.limit,
            Some(ws.root.as_path()),
        )
        .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Search indexed symbols by name pattern. \
        FTS5-backed search across symbol names, signatures, and docstrings.")]
    pub async fn sutra_grep(
        &self,
        Parameters(args): Parameters<GrepArgs>,
    ) -> Result<String, ErrorData> {
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::grep::handle_with_freshness(
            &db,
            &args.pattern,
            args.kind.as_deref(),
            args.limit,
            Some(ws.root.as_path()),
        )
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
        let mut result =
            tools::read::handle(&db, &ws.root, &args.symbol, args.context_lines).map_err(sutra_to_rmcp)?;

        // Tier-2 stale refusal: withhold content when index is stale
        let freshness = self.freshness(&db);
        if freshness["is_stale"].as_bool() == Some(true) {
            if let Some(obj) = result.as_object_mut() {
                obj.remove("content");
                obj.insert(
                    "refused".into(),
                    serde_json::json!("content withheld: index is stale"),
                );
                obj.insert(
                    "next_action".into(),
                    serde_json::json!("Run sutra_parse to refresh, then retry."),
                );
            }
        }

        self.wrap_response(&db, result)
    }

    #[tool(description = "Blast radius analysis for a symbol. Counts direct callers, \
        runs transitive BFS (depth 3), and computes risk level (low/medium/high). \
        Also acknowledges the file for the modification guard.")]
    pub async fn sutra_impact(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<String, ErrorData> {
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::impact::handle(&db, &args.symbol).map_err(sutra_to_rmcp)?;
        if let Some(file_path) = result["file"].as_str() {
            guard::touch_ack(&ws.root, file_path);
        }
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

    #[tool(description = "All usages of a symbol across the codebase. \
        Groups references by file with line numbers. Requires analysis tier.")]
    pub async fn sutra_refs(
        &self,
        Parameters(args): Parameters<RefsArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::refs::handle(&db, &args.symbol).map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Call hierarchy for a function. \
        direction=callers (default) or callees. BFS to depth (default 1, max 3). \
        Requires analysis tier.")]
    pub async fn sutra_calls(
        &self,
        Parameters(args): Parameters<CallsArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::calls::handle(
            &db,
            &args.symbol,
            args.direction.as_deref(),
            args.depth,
        )
        .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Trace call chains through the codebase. \
        direction=forward (default): finds paths from entry points to the symbol. \
        direction=backward: finds paths from the symbol to leaf functions. \
        Detects and marks cycles. Entry points: main, Dart lifecycle methods, \
        or any symbol with zero callers. Requires analysis tier.")]
    pub async fn sutra_trace(
        &self,
        Parameters(args): Parameters<TraceArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::trace::handle(
            &db,
            &args.symbol,
            args.direction.as_deref(),
            args.limit,
        )
        .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Blast radius of a git diff. \
        Shows changed files, affected symbols, and their callers. Requires analysis tier.")]
    pub async fn sutra_diff_impact(
        &self,
        Parameters(args): Parameters<DiffImpactArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::diff_impact::handle(
            &db,
            &ws.root,
            args.base.as_deref(),
            args.head.as_deref(),
        )
        .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Composite PR risk score (0.0–1.0) for a git diff. \
        Combines blast_radius, complexity, churn, and volume signals with \
        documented weights. Returns per-signal breakdown and top-N riskiest \
        changed symbols. Requires analysis tier.")]
    pub async fn sutra_pr_risk(
        &self,
        Parameters(args): Parameters<PrRiskArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::pr_risk::handle(
            &db,
            &ws.root,
            args.base.as_deref(),
            args.head.as_deref(),
        )
        .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Git history of a symbol's file with commit classification \
        (feature, bugfix, refactor, test, docs, chore, performance, unknown). \
        Uses --follow for rename tracking. Requires analysis tier.")]
    pub async fn sutra_provenance(
        &self,
        Parameters(args): Parameters<ProvenanceArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::provenance::handle(&db, &ws.root, &args.symbol)
            .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Files that historically change together with a given file in git history. \
        Requires analysis tier.")]
    pub async fn sutra_cochange(
        &self,
        Parameters(args): Parameters<CochangeArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result =
            tools::cochange::handle(&db, &ws.root, &args.path, args.window_days)
                .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Find dead symbols (zero inbound references) and unreachable files \
        (zero importers). Automatically excludes #[test]/#[bench] functions, items inside \
        #[cfg(test)] modules, #[no_mangle]/FFI entrypoints, and integration test files. \
        Requires analysis tier.")]
    pub async fn sutra_dead(
        &self,
        Parameters(args): Parameters<DeadArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::dead::handle(
            &db,
            args.path_prefix.as_deref(),
            args.include_pub.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Riskiest files ranked by git churn × blast radius × complexity. \
        Requires analysis tier.")]
    pub async fn sutra_hotspots(
        &self,
        Parameters(args): Parameters<HotspotsArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::hotspots::handle_with_freshness(
            &db,
            &ws.root,
            args.window_days,
            args.limit,
            true,
        )
        .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Per-file health score (0-100). Combines blast radius, complexity, \
        fan-in, dead-symbol ratio, and PageRank into a single maintainability index. \
        Worst files first. Requires analysis tier.")]
    pub async fn sutra_file_health(
        &self,
        Parameters(args): Parameters<FileHealthArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::file_health::handle_with_freshness(
            &db,
            args.path.as_deref(),
            args.limit,
            Some(ws.root.as_path()),
        )
        .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Compare two parse snapshots and return per-metric deltas. \
        Useful for CI gates and pre/post-refactor checks. \
        Defaults to comparing the two most recent snapshots.")]
    pub async fn sutra_trend(
        &self,
        Parameters(args): Parameters<TrendArgs>,
    ) -> Result<String, ErrorData> {
        let _ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
        let result = tools::trend::handle(
            &db,
            args.from.as_deref(),
            args.to.as_deref(),
        )
        .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Multi-axis composite query. AND-intersects filters (kind, \
        min_complexity, min_churn, calls_to, file_glob, name_regex) and ranks results \
        by importance (PageRank), complexity, or churn. Each result includes per-axis \
        values. Requires analysis tier for calls_to.")]
    pub async fn sutra_winnow(
        &self,
        Parameters(args): Parameters<WinnowArgs>,
    ) -> Result<String, ErrorData> {
        if args.calls_to.is_some() {
            self.require_analysis()?;
        }
        let ws = self.resolve_workspace(&args.workspace)?;
        let db = self.get_db(&args.workspace)?;
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
        let result = tools::winnow::handle(&db, &ws.root, &filter)
            .map_err(sutra_to_rmcp)?;
        self.wrap_response(&db, result)
    }

    #[tool(description = "Register a workspace root and start indexing. \
        Derives a workspace id from the directory name. If the workspace is already \
        registered, triggers a reparse. Parsing runs in the background — other \
        tools become available as soon as the parse completes.")]
    pub async fn sutra_add_root(
        &self,
        Parameters(args): Parameters<AddRootArgs>,
    ) -> Result<String, ErrorData> {
        let root = PathBuf::from(&args.path);
        if !root.is_absolute() || !root.is_dir() {
            return Err(ErrorData::new(
                rmcp::model::ErrorCode(crate::error::codes::INVALID_PARAMS),
                format!("path must be an absolute directory that exists: {}", args.path),
                None,
            ));
        }

        let dir_name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let ws_id = dir_name.to_lowercase().replace(' ', "-");
        let languages = args.languages.unwrap_or_else(|| vec!["rust".into(), "dart".into()]);

        let entry = workspace::WorkspaceEntry {
            id: ws_id.clone(),
            root: root.clone(),
            languages: languages.clone(),
        };

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

        {
            let mut parsing = self.parsing_in_progress.lock();
            if parsing.contains(&ws_id) {
                return serde_json::to_string_pretty(&serde_json::json!({
                    "workspace": ws_id,
                    "root": root.display().to_string(),
                    "status": "parse already in progress",
                })).map_err(json_to_rmcp);
            }
            parsing.insert(ws_id.clone());
        }

        let db = self.get_db(&ws_id)?;
        let config = Arc::clone(&self.config);
        let ws_id_bg = ws_id.clone();
        let parsing_flag = Arc::clone(&self.parsing_in_progress);
        tokio::spawn(async move {
            let result = crate::pipeline::parse_workspace(&entry, &db, &config).await;
            parsing_flag.lock().remove(&ws_id_bg);
            match result {
                Ok(snap) => {
                    tracing::info!(
                        "add_root parse complete for {}: {}/{} files changed, {} symbols in {}ms",
                        ws_id_bg, snap.files_parsed, snap.files_walked, snap.symbols_extracted, snap.duration_ms
                    );
                }
                Err(e) => {
                    tracing::error!("add_root parse failed for {}: {e}", ws_id_bg);
                }
            }
        });

        let status = if already_exists { "exists, reparsing" } else { "registered, parsing" };
        serde_json::to_string_pretty(&serde_json::json!({
            "workspace": ws_id,
            "root": root.display().to_string(),
            "languages": languages,
            "status": status,
        })).map_err(json_to_rmcp)
    }

    #[tool(description = "Register a workspace and return its status. \
        Preferred session-start call — tries the daemon first (POST /workspaces), \
        falls back to local parse if daemon is not running. \
        Returns mode (daemon|local), status, freshness, file/symbol counts.")]
    pub async fn sutra_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
    ) -> Result<String, ErrorData> {
        let root = PathBuf::from(&args.path);
        if !root.is_absolute() || !root.is_dir() {
            return Err(ErrorData::new(
                rmcp::model::ErrorCode(crate::error::codes::INVALID_PARAMS),
                format!("path must be an absolute directory that exists: {}", args.path),
                None,
            ));
        }

        let dir_name = root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("workspace");
        let ws_id = dir_name.to_lowercase().replace(' ', "-");
        let languages = args.languages.unwrap_or_else(|| vec!["rust".into(), "dart".into()]);

        let daemon_url = &self.config.listen_addr;

        // Try daemon path first
        if let Ok(resp) = self.try_daemon_register(daemon_url, &args.path, &languages).await {
            return Ok(resp);
        }

        // Fallback: local mode — register + parse
        let entry = workspace::WorkspaceEntry {
            id: ws_id.clone(),
            root: root.clone(),
            languages: languages.clone(),
        };

        {
            let mut config = self.workspaces.write();
            if !config.workspace.iter().any(|w| w.id == ws_id) {
                let _ = workspace::add_workspace(&self.config.workspaces_path, entry.clone());
                config.workspace.push(entry.clone());
            }
        }

        let db = self.get_db(&ws_id)?;

        let needs_parse = db.last_parse_time().ok().flatten().is_none();
        if needs_parse {
            let _ = crate::pipeline::parse_workspace(&entry, &db, &self.config).await;
        }

        let files = db.all_files().unwrap_or_default();
        let sym_counts = db.symbol_counts_by_file().unwrap_or_default();
        let total_symbols: i64 = sym_counts.values().sum();
        let freshness = self.freshness(&db);
        let smriti_connected = self.check_smriti_connected();

        let status = if files.is_empty() { "empty" } else { "ready" };

        serde_json::to_string_pretty(&serde_json::json!({
            "workspace": ws_id,
            "root": root.display().to_string(),
            "mode": "local",
            "status": status,
            "last_parse": freshness["as_of"],
            "is_stale": freshness["is_stale"],
            "files": files.len(),
            "symbols": total_symbols,
            "smriti_connected": smriti_connected,
        })).map_err(json_to_rmcp)
    }
}

impl SutraServer {
    async fn try_daemon_register(
        &self,
        addr: &str,
        path: &str,
        languages: &[String],
    ) -> std::result::Result<String, ()> {
        let base = format!("http://{addr}");
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .map_err(|_| ())?;

        // POST /workspaces to register
        let post_resp = client
            .post(format!("{base}/workspaces"))
            .json(&serde_json::json!({
                "path": path,
                "languages": languages,
            }))
            .send()
            .await
            .map_err(|_| ())?;

        if !post_resp.status().is_success() && post_resp.status().as_u16() != 409 {
            return Err(());
        }

        let post_json: serde_json::Value = post_resp.json().await.map_err(|_| ())?;
        let ws_id = post_json["id"].as_str().unwrap_or("unknown").to_string();

        // Poll /status for up to 10s until workspace has parse data
        for _ in 0..10 {
            if let Ok(status) = client.get(format!("{base}/status")).send().await {
                if let Ok(json) = status.json::<serde_json::Value>().await {
                    if let Some(workspaces) = json["workspaces"].as_array() {
                        if let Some(ws) = workspaces.iter().find(|w| w["id"] == ws_id) {
                            let file_count = ws["file_count"].as_i64().unwrap_or(0);
                            let last_parse = ws["last_parse_time"].as_str().unwrap_or("");
                            if file_count > 0 || !last_parse.is_empty() {
                                let smriti_connected = self.check_smriti_connected();
                                let is_stale = last_parse.is_empty();
                                let status_str = if file_count == 0 { "empty" } else { "ready" };
                                return serde_json::to_string_pretty(&serde_json::json!({
                                    "workspace": ws_id,
                                    "root": ws["root"],
                                    "mode": "daemon",
                                    "status": status_str,
                                    "last_parse": last_parse,
                                    "is_stale": is_stale,
                                    "files": file_count,
                                    "symbols": ws["symbol_count"],
                                    "smriti_connected": smriti_connected,
                                })).map_err(|_| ());
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Err(())
    }

    fn check_smriti_connected(&self) -> bool {
        let smriti_db = std::env::var("SUTRA_SMRITI_DB")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                PathBuf::from(home).join(".smriti").join("index.db")
            });
        smriti_db.exists()
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
             sutra_diff_impact, sutra_cochange, sutra_pr_risk, sutra_provenance, sutra_trace. \
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
