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
pub struct WorkspaceArgs {
    pub workspace: String,
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

// ---------------------------------------------------------------------------
// Args structs re-exported from tool modules
// ---------------------------------------------------------------------------

use crate::tools::calls::CallsArgs;
use crate::tools::cochange::CochangeArgs;
use crate::tools::commit_manifest::CommitManifestArgs;
use crate::tools::components::ComponentsArgs;
use crate::tools::dead::DeadArgs;
use crate::tools::deps::DepsArgs;
use crate::tools::diff_impact::DiffImpactArgs;
use crate::tools::duplicates::DuplicatesArgs;
use crate::tools::explore::ExploreArgs;
use crate::tools::file_health::FileHealthArgs;
use crate::tools::find::FindArgs;
use crate::tools::grep::GrepArgs;
use crate::tools::hotspots::HotspotsArgs;
use crate::tools::impact::ImpactArgs;
use crate::tools::map::MapArgs;
use crate::tools::outline::OutlineArgs;
use crate::tools::pr_risk::PrRiskArgs;
use crate::tools::provenance::ProvenanceArgs;
use crate::tools::read::ReadArgs;
use crate::tools::refs::RefsArgs;
use crate::tools::remember::RememberArgs;
use crate::tools::resolve::ResolveArgs;
use crate::tools::review::ReviewArgs;
use crate::tools::similar::SimilarArgs;
use crate::tools::tools_meta::ToolsMetaArgs;
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
    analysis_enabled: Arc<AtomicBool>,
    parse_coord: ParseCoordinator,
    dd_engines: Arc<Mutex<HashMap<String, Arc<DdEngine>>>>,
    lessons_db: Arc<LessonsDb>,
    tool_router: ToolRouter<Self>,
}

impl Clone for SutraServer {
    fn clone(&self) -> Self {
        Self {
            db_cache: Arc::clone(&self.db_cache),
            config: Arc::clone(&self.config),
            workspaces: Arc::clone(&self.workspaces),
            analysis_enabled: Arc::clone(&self.analysis_enabled),
            parse_coord: self.parse_coord.clone(),
            dd_engines: Arc::clone(&self.dd_engines),
            lessons_db: Arc::clone(&self.lessons_db),
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
            analysis_enabled: Arc::new(AtomicBool::new(false)),
            parse_coord,
            dd_engines: Arc::new(Mutex::new(HashMap::new())),
            lessons_db,
            tool_router: Self::tool_router(),
        }
    }

    pub fn with_dd_engines(mut self, engines: Arc<Mutex<HashMap<String, Arc<DdEngine>>>>) -> Self {
        self.dd_engines = engines;
        self
    }

    fn get_dd_engine(&self, ws_id: &str) -> Arc<DdEngine> {
        let mut engines = self.dd_engines.lock();
        engines
            .entry(ws_id.to_string())
            .or_insert_with(|| {
                Arc::new(DdEngine::new(Duration::from_secs(
                    self.config.constraints_idle_timeout_sec,
                )))
            })
            .clone()
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
        let ws = self.resolve_workspace(ws_id)?;
        tools::get_or_open_db(&self.db_cache, &ws, &self.config.db_dir).map_err(sutra_to_rmcp)
    }

    fn tool_context(&self, ws_id: &str) -> std::result::Result<tools::ToolContext, ErrorData> {
        let ws = self.resolve_workspace(ws_id)?;
        let db = tools::get_or_open_db(&self.db_cache, &ws, &self.config.db_dir)
            .map_err(sutra_to_rmcp)?;
        let response_freshness = self.freshness(&db, &ws.root);
        Ok(tools::ToolContext::new(
            db,
            ws.root,
            true,
            response_freshness,
        ))
    }

    fn require_analysis(&self) -> std::result::Result<(), ErrorData> {
        if !self
            .analysis_enabled
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(ErrorData::new(
                rmcp::model::ErrorCode(crate::error::codes::INVALID_PARAMS),
                "Analysis tier not enabled. Call sutra_tools with enable: [\"analysis\"] first."
                    .to_string(),
                None,
            ));
        }
        Ok(())
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
        let (as_of, is_stale) = crate::freshness::is_workspace_stale(
            db,
            workspace_root,
            self.config.stale_threshold_sec,
        );

        let parsing = self.parse_coord.is_locked(db.workspace_id());

        let mut val = serde_json::json!({
            "as_of": as_of,
            "is_stale": is_stale,
        });
        if parsing {
            val["parsing_in_progress"] = serde_json::Value::Bool(true);
        }
        val
    }

    async fn await_parse(&self, ws_id: &str) {
        let lock = self.parse_coord.lock_for(ws_id);
        let _guard = lock.lock().await;
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
        let ctx = self.tool_context(&args.workspace)?;
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
        qualified names, kinds, line ranges, and signatures."
    )]
    pub async fn sutra_outline(
        &self,
        Parameters(args): Parameters<OutlineArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let compact = args.compact.unwrap_or(false);
        let result =
            tools::outline::handle(ctx.db(), &args.path, compact).map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "List discovered architectural components. Compact mode (default) returns name, file_count, top 3 anchors, and concept_density. Pass compact=false for full detail with UUIDs, complete file lists, and anchor rationale."
    )]
    pub async fn sutra_components(
        &self,
        Parameters(args): Parameters<ComponentsArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let compact = args.compact.unwrap_or(true);
        let result = tools::components::handle(ctx.db(), compact).map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Convention-aware orientation for a component or file scope. \
        Returns observed conventions, \
        active constraints (with violations and constraint waivers), \
        and health findings. \
        Scope can be a component name, component ID, or file path."
    )]
    pub async fn sutra_orient(
        &self,
        Parameters(args): Parameters<tools::orient::OrientArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let dd = self.get_dd_engine(&args.workspace);
        let registry = crate::parser::adapter::default_registry();
        let result = tools::orient::handle(
            ctx.db(),
            &args.scope,
            ctx.workspace_root(),
            Some(&dd),
            Some(&self.lessons_db),
            &registry,
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "List discovered conventions. \
        Actions: list (all conventions).")]
    pub async fn sutra_conventions(
        &self,
        Parameters(args): Parameters<tools::conventions::ConventionsArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::conventions::handle(ctx.db(), &args).map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Manage architectural constraints and their violations. \
        Actions: list (all constraints with kind, severity, name, provenance, scope, waiver count), \
        violations (current constraint violations — covers forbidden_dep, \
        boundary, no_cycles, and the external-crate kinds forbidden_external / confined_external \
        [use-statement + Cargo manifest signals], and max_fan_in [fan-in threshold from DB rollups]), \
        waive (constraint_id, file_path, rationale, waived_by; optional constraint_name, \
        symbol_qualified_name), \
        unwaive (waiver_id)."
    )]
    pub async fn sutra_constraints(
        &self,
        Parameters(args): Parameters<tools::constraints::ConstraintsArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let dd = self.get_dd_engine(&args.workspace);
        let result = tools::constraints::handle(ctx.db(), ctx.workspace_root(), Some(&dd), &args)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Resolve a vocabulary term to code locations. Searches aliases \
        (from .sutra/aliases.toml), component names, and semantic anchor names. \
        Returns matches in priority order with orphan detection."
    )]
    pub async fn sutra_resolve(
        &self,
        Parameters(args): Parameters<ResolveArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::resolve::handle(ctx.db(), &args.query).map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Jump to a symbol definition by name. Three-tier search: \
        exact short_name, exact qualified_name, then FTS5 fuzzy. \
        Returns compact results by default; pass detail=true for signatures and visibility."
    )]
    pub async fn sutra_find(
        &self,
        Parameters(args): Parameters<FindArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::find::handle_ctx(
            &ctx,
            &args.name,
            args.kind.as_deref(),
            args.limit,
            args.detail.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Search indexed symbols by name pattern. \
        FTS5-backed search across symbol names, signatures, and docstrings. \
        Returns compact results by default; pass detail=true for signatures and docstrings.")]
    pub async fn sutra_grep(
        &self,
        Parameters(args): Parameters<GrepArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::grep::handle_ctx(
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
        description = "Explore a topic in the codebase. Returns a ranked list of matching \
        symbols with literal sutra_read fetch instructions and a strategy recommendation. \
        One call replaces iterative map/outline/grep exploration."
    )]
    pub async fn sutra_explore(
        &self,
        Parameters(args): Parameters<ExploreArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let budget = args.budget.unwrap_or(10);
        let result =
            tools::explore::handle(ctx.db(), &args.query, budget).map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Read a symbol's source code from disk with line numbers. \
        Includes context lines around the symbol. Returns stale warning if file was deleted. \
        Default 500-line cap; use full=true or limit=N to override."
    )]
    pub async fn sutra_read(
        &self,
        Parameters(args): Parameters<ReadArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
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

    #[tool(description = "Store or cite a code-anchored lesson. \
        Store mode: provide text + location_anchors, each either a bare string \
        (symbol name or file path — kind is inferred) or {kind, value} where kind \
        is symbol|file|import_pattern|directory; \
        pass workspace to auto-enrich with import-pattern anchors, directory anchors, \
        and language/technology categories from the workspace graph. \
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
        (category, symbol anchor, verified). Returns matching lessons ranked by relevance."
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
        Also acknowledges the file for the modification guard."
    )]
    pub async fn sutra_impact(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::impact::handle(
            ctx.db(),
            &args.symbol,
            args.explain.unwrap_or(false),
            Some(&self.lessons_db),
            ctx.workspace_root(),
        )
        .map_err(sutra_to_rmcp)?;
        if let Some(file_path) = result["file"].as_str() {
            guard::touch_ack(ctx.workspace_root(), file_path);
        }
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "File dependency graph from import edges. \
        If path given, BFS from that file to depth. \
        If cycles=true, detect import cycle groups (SCCs) instead of listing edges.")]
    pub async fn sutra_deps(
        &self,
        Parameters(args): Parameters<DepsArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let cycles = args.cycles.unwrap_or(false);
        let result = tools::deps::handle(ctx.db(), args.path.as_deref(), args.depth, cycles)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Trigger a workspace reparse. Use after editing files to get \
        fresh results from other tools."
    )]
    pub async fn sutra_parse(
        &self,
        Parameters(args): Parameters<WorkspaceArgs>,
    ) -> Result<String, ErrorData> {
        let ws = self.resolve_workspace(&args.workspace)?;
        let ws_root = ws.root.clone();
        let db = self.get_db(&args.workspace)?;
        let lock = self.parse_coord.lock_for(&args.workspace);
        let _guard = lock.lock().await;
        let config = Arc::clone(&self.config);
        let db_bg = Arc::clone(&db);
        let result = tokio::task::spawn_blocking(move || {
            let cancel = AtomicBool::new(false);
            let registry = crate::parser::adapter::default_registry();
            tools::parse::handle(&ws, &db_bg, &config, &cancel, &registry)
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

    #[tool(
        description = "Manage tool tiers. Enable or disable the analysis tier \
        (sutra_refs, sutra_calls, sutra_diff_impact, sutra_commit_manifest, sutra_cochange, sutra_review). \
        Use list=true to see available tiers and their status."
    )]
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
        to_compact_json(result)
    }

    #[tool(description = "All usages of a symbol across the codebase. \
        Groups references by file with line numbers. Optional context_kind filter \
        (e.g. \"construction\", \"call\", \"type_use\") to narrow results. Requires analysis tier.")]
    pub async fn sutra_refs(
        &self,
        Parameters(args): Parameters<RefsArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::refs::handle(ctx.db(), &args.symbol, args.context_kind.as_deref())
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Call hierarchy for a function. \
        direction=callers (default) or callees. BFS to depth (default 1, max 3). \
        Requires analysis tier.")]
    pub async fn sutra_calls(
        &self,
        Parameters(args): Parameters<CallsArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ctx = self.tool_context(&args.workspace)?;
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
        or any symbol with zero callers. Requires analysis tier.")]
    pub async fn sutra_trace(
        &self,
        Parameters(args): Parameters<TraceArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ctx = self.tool_context(&args.workspace)?;
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
        Shows changed files, affected symbols, and their callers. Requires analysis tier.")]
    pub async fn sutra_diff_impact(
        &self,
        Parameters(args): Parameters<DiffImpactArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        self.await_parse(&args.workspace).await;
        let ctx = self.tool_context(&args.workspace)?;
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
        Defaults to branch range (merge-base..HEAD). Max 50 commits. \
        Requires analysis tier.")]
    pub async fn sutra_commit_manifest(
        &self,
        Parameters(args): Parameters<CommitManifestArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        self.await_parse(&args.workspace).await;
        let ctx = self.tool_context(&args.workspace)?;
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
        changed symbols. Requires analysis tier.")]
    pub async fn sutra_pr_risk(
        &self,
        Parameters(args): Parameters<PrRiskArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        self.await_parse(&args.workspace).await;
        let ctx = self.tool_context(&args.workspace)?;
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
        Uses --follow for rename tracking. Requires analysis tier."
    )]
    pub async fn sutra_provenance(
        &self,
        Parameters(args): Parameters<ProvenanceArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::provenance::handle(ctx.db(), ctx.workspace_root(), &args.symbol)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Entities that historically change together in git history. \
        granularity='file' (default): file-level co-change by path. \
        granularity='function': function-level co-change by qualified symbol name. \
        Reports both jaccard and confidence metrics for function granularity. \
        Requires analysis tier."
    )]
    pub async fn sutra_cochange(
        &self,
        Parameters(args): Parameters<CochangeArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ctx = self.tool_context(&args.workspace)?;
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
        diff: \"branch\" (default, against main merge-base), \"staged\", \"unstaged\", \
        or a commit spec — \"abc123..def456\" for a range, \"abc123\" for a single commit. \
        Requires analysis tier."
    )]
    pub async fn sutra_review(
        &self,
        Parameters(args): Parameters<ReviewArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        self.await_parse(&args.workspace).await;
        let ctx = self.tool_context(&args.workspace)?;
        let dd = self.get_dd_engine(&args.workspace);
        let result = tools::review::handle(
            ctx.db(),
            ctx.workspace_root(),
            args.diff.as_deref(),
            Some(&dd),
            args.explain.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Find dead symbols (zero inbound references) and unreachable files \
        (zero importers). Automatically excludes #[test]/#[bench] functions, items inside \
        #[cfg(test)] modules, #[no_mangle]/FFI entrypoints, and integration test files. \
        Requires analysis tier."
    )]
    pub async fn sutra_dead(
        &self,
        Parameters(args): Parameters<DeadArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::dead::handle(
            ctx.db(),
            args.path_prefix.as_deref(),
            args.include_pub.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Riskiest files ranked by git churn × blast radius × complexity. \
        Requires analysis tier."
    )]
    pub async fn sutra_hotspots(
        &self,
        Parameters(args): Parameters<HotspotsArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::hotspots::handle_ctx(&ctx, args.window_days, args.limit)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Per-file and per-component health report. Returns derived health scores \
        (1.0-10.0), active findings with full detail, category deductions, and component \
        instability (Martin's Ce/(Ca+Ce)). Filter by file path or component name. \
        Default mode='actionable' shows only files with findings; mode='all' includes everything. \
        Worst files first. Requires analysis tier."
    )]
    pub async fn sutra_file_health(
        &self,
        Parameters(args): Parameters<FileHealthArgs>,
    ) -> Result<String, ErrorData> {
        self.require_analysis()?;
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::file_health::handle_ctx(
            &ctx,
            args.path.as_deref(),
            args.limit,
            args.mode.as_deref(),
            args.component.as_deref(),
            args.explain.unwrap_or(false),
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Compare two parse snapshots with per-file and per-component health deltas, \
        or query a single file's health history over time. \
        Defaults to comparing the two most recent snapshots. \
        Set 'path' to get a per-file time series instead of a comparison."
    )]
    pub async fn sutra_trend(
        &self,
        Parameters(args): Parameters<TrendArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::trend::handle(ctx.db(), &args).map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Multi-axis composite query. AND-intersects filters (kind, \
        min_complexity, min_churn, calls_to, file_glob, name_regex) and ranks results \
        by importance (PageRank), complexity, or churn. Each result includes per-axis \
        values. Requires analysis tier for calls_to."
    )]
    pub async fn sutra_winnow(
        &self,
        Parameters(args): Parameters<WinnowArgs>,
    ) -> Result<String, ErrorData> {
        if args.calls_to.is_some() {
            self.require_analysis()?;
        }
        let ctx = self.tool_context(&args.workspace)?;
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
        description = "Near-duplicate function detection via structural similarity of HRR strip \
        vectors. Returns pattern families — groups of 3+ functions with near-identical AST \
        structure. Configurable similarity threshold (default 0.85) and minimum group size \
        (default 3)."
    )]
    pub async fn sutra_duplicates(
        &self,
        Parameters(args): Parameters<DuplicatesArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::duplicates::handle(ctx.db(), args.threshold, args.min_group)
            .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(
        description = "Find structurally similar functions using HRR vector similarity. \
            Mode 'strip' (default) finds functions with the same AST shape regardless of \
            identifier names — useful for finding copy-paste variants. Mode 'embed' finds \
            functions similar in both structure and naming."
    )]
    pub async fn sutra_similar(
        &self,
        Parameters(args): Parameters<SimilarArgs>,
    ) -> Result<String, ErrorData> {
        let ctx = self.tool_context(&args.workspace)?;
        let result = tools::similar::handle(
            ctx.db(),
            &args.symbol,
            args.mode.as_deref(),
            args.limit,
            args.threshold,
        )
        .map_err(sutra_to_rmcp)?;
        to_compact_json(ctx.wrap(result))
    }

    #[tool(description = "Register a workspace root and start indexing. \
        Derives a workspace id from the directory name. If the workspace is already \
        registered, triggers a reparse. Parsing runs in the background — other \
        tools become available as soon as the parse completes.")]
    pub async fn sutra_add_root(
        &self,
        Parameters(args): Parameters<AddRootArgs>,
    ) -> Result<String, ErrorData> {
        let (ws_id, entry, already_exists) = self.register_workspace(&args.path, args.languages)?;

        let lock = self.parse_coord.lock_for(&ws_id);
        let Ok(guard) = lock.clone().try_lock_owned() else {
            return to_compact_json(serde_json::json!({
                "workspace": ws_id,
                "root": entry.root.display().to_string(),
                "status": "parse already in progress",
            }));
        };

        let db = self.get_db(&ws_id)?;
        let config = Arc::clone(&self.config);
        let dd_engines = Arc::clone(&self.dd_engines);
        let ws_id_bg = ws_id.clone();
        let entry_bg = entry.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let result = tokio::task::spawn_blocking(move || {
                let cancel = std::sync::atomic::AtomicBool::new(false);
                let registry = crate::parser::adapter::default_registry();
                crate::pipeline::parse_workspace(&entry_bg, &db, &config, &cancel, &registry)
            })
            .await;
            match result {
                Ok(Ok(snap)) => {
                    if let Some(engine) = dd_engines.lock().get(&ws_id_bg) {
                        engine.invalidate();
                    }
                    tracing::info!(
                        "add_root parse complete for {}: {}/{} files changed, {} symbols in {}ms",
                        ws_id_bg,
                        snap.files_parsed,
                        snap.files_walked,
                        snap.symbols_extracted,
                        snap.duration_ms
                    );
                }
                Ok(Err(e)) => {
                    tracing::error!("add_root parse failed for {}: {e}", ws_id_bg);
                }
                Err(e) => {
                    tracing::error!("add_root parse panicked for {}: {e}", ws_id_bg);
                }
            }
        });

        let status = if already_exists {
            "exists, reparsing"
        } else {
            "registered, parsing"
        };
        to_compact_json(serde_json::json!({
            "workspace": ws_id,
            "root": entry.root.display().to_string(),
            "languages": entry.languages,
            "status": status,
        }))
    }

    #[tool(description = "Register a workspace and return its status. \
        Returns status, freshness, file/symbol counts.")]
    pub async fn sutra_status(
        &self,
        Parameters(args): Parameters<StatusArgs>,
    ) -> Result<String, ErrorData> {
        let (ws_id, entry, _) = self.register_workspace(&args.path, args.languages)?;

        let db = self.get_db(&ws_id)?;

        let needs_parse = db.last_parse_time().ok().flatten().is_none();
        if needs_parse {
            let entry_bg = entry.clone();
            let db_bg = Arc::clone(&db);
            let config_bg = Arc::clone(&self.config);
            let _ = tokio::task::spawn_blocking(move || {
                let cancel = AtomicBool::new(false);
                let registry = crate::parser::adapter::default_registry();
                crate::pipeline::parse_workspace(&entry_bg, &db_bg, &config_bg, &cancel, &registry)
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
}

// ---------------------------------------------------------------------------
// ServerHandler
// ---------------------------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SutraServer {
    fn get_info(&self) -> ServerInfo {
        let mut tool_names: Vec<&str> = self.tool_router.map.keys().map(|k| k.as_ref()).collect();
        tool_names.sort_unstable();
        let roster = tool_names.join(", ");
        let instructions = format!(
            "sutra v{version} — code intelligence for manas. \
             {count} tools: {roster}. \
             Call sutra_help() for workflow recipes. \
             All responses include as_of timestamp and is_stale indicator.",
            version = env!("CARGO_PKG_VERSION"),
            count = tool_names.len(),
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
