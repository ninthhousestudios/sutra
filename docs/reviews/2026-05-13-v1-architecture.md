# Architecture Review — sutra v1 Release Gate

Date: 2026-05-13
Commit: d66da87 (HEAD of main)
Scope: Full codebase (88 files, 1,297 symbols). Release gate for sutra v1.
Build: passing (311 tests, 0 failures). Clippy: 17 warnings, no errors.

---

## Module map assessment

**20 top-level modules** declared in `src/lib.rs`. After the v0.2.0 checkpoint review, several structural improvements landed: `graph.rs` was extracted, `post_parse_sequence` consolidated duplicated orchestration, and tool arg structs were moved into their respective `src/tools/*.rs` modules. The experimental modules (`hdc`, `hrr`, `analogy`, `multiscale`) and spike binaries have been removed from disk (stale index entries remain harmless).

### Well-bounded modules

- **`dd/`** (608 lines, 3 files) — Clean boundary. Worker thread communication via `Command`/`Response` enums over crossbeam channels. Public interface is just `DdEngine` with `ingest`, `update`, `query_*`, and `evict_if_idle`. No DD types leak outside the module. The only inward dependency is `crate::rules::ForbiddenDep` for rule matching, which is appropriate.

- **`fca/`** (1,262 lines, 4 files) — Good encapsulation. `FcaEngine`, `Convention`, `ConventionViolation`, and `SymbolAttrs` are the public surface. Internal `bitset` and `context` modules are private. The FCA code only depends on `crate::db::SymbolRow` (for attribute extraction) and `crate::rules::ConventionsConfig` (for suppressions). Well-tested (17 tests in engine.rs alone).

- **`parser/`** (174 + 798 + 473 + 240 = 1,685 lines, 4 files) — Solid boundary at the module level. `mod.rs` defines the language-neutral data types (`ParseResult`, `ExtractedSymbol`, `ExtractedRef`, `ExtractedImport`) and dispatches to language-specific parsers. Language parsers are private to the module.

- **`rules.rs`** (53 lines of production code) — Small, focused, well-shaped. Defines the config types and loads `.sutra/rules.toml`. Consumed by `dd/engine.rs` and `fca/engine.rs` without coupling them to each other.

- **`diagnostics.rs`** (82 lines) — Clean. Defines `Diagnostic` enum with `suggest_next_query()`. Used by tool handlers.

- **`freshness.rs`** (179 lines) — Coherent. `FileStatus`, `FreshnessCounts`, `SearchTier`, `FreshnessLevel` all serve the same purpose. Well-tested.

### Modules with boundary concerns

- **`db.rs`** (1,323 lines, 67 symbols) — Still the largest module. The v0.2.0 review noted this was shallow (many thin query wrappers). The convention-related methods (`upsert_convention`, `all_conventions`, `suppress_convention`, `delete_stale_conventions`) were added for v1 without deepening the module. See candidate 1.

- **`mcp.rs`** (1,013 lines, 54 symbols) — The arg structs moved out, but the file remains large because each tool handler is a thin async method that still follows the identical `resolve_workspace` / `get_db` / `require_analysis` / `tools::*::handle(...)` / `wrap_response` pattern. The v1.1 MCP facade consolidation (PRD milestone 7) will address this, so no action needed now.

- **`tools/review.rs`** (441 lines) — This is the v1 review compositor. It orchestrates DD, FCA, git, and risk scoring in a single file. See candidate 2.

- **`pipeline.rs`** (652 lines) — Down from 812 lines at the v0.2.0 checkpoint thanks to `graph.rs` extraction and `post_parse_sequence` consolidation. Still the highest-churn file (21 commits in 90d). Acceptable for now.

- **`main.rs`** (662 lines) — Contains CLI definition, serve/parse/workspace/guard/health/install-services commands, and the MCP/HTTP wiring. The guard install/uninstall logic (lines 518-648) has nothing to do with server startup. See candidate 5.

---

## Deepening candidates

### 1. Split Db into schema/migration layer and query facades

**Location:** `src/db.rs` (1,323 lines, 67 symbols)

**What's wrong:** `Db` is a single struct with 45+ public methods spanning four distinct responsibilities: (a) database lifecycle and migrations (lines 155-363), (b) file/symbol/ref CRUD (lines 383-977), (c) graph-oriented query methods like `import_edges`, `all_resolved_refs`, `all_symbol_file_map` that exist solely to serve `graph.rs` and `pipeline.rs` (lines 1042-1073), and (d) convention persistence for FCA (lines 1155-1242). Every new subsystem that needs data adds more methods here, growing the interface linearly. The migration system alone (content-hash verification, pre-runner detection, retroactive registration) is 210 lines of careful, rarely-touched code that is interleaved with volatile query methods.

**What to do:** Extract the migration runner into its own module (or at minimum a separate impl block in a `db/migrations.rs` file). Consider grouping the graph-oriented query methods (`import_edges`, `all_resolved_refs`, `all_symbol_file_map`, `batch_update_file_pagerank`, `batch_update_symbol_pagerank`, `update_rollups`) behind a `db::graph_queries` facade or moving them into `graph.rs` as private helpers that take `&Connection`. The convention methods could similarly live closer to the FCA module. The goal is that `Db`'s public surface shrinks to lifecycle + CRUD, with domain-specific queries owned by the domains that use them.

**Effort:** Medium (half-day). The migration extraction is mechanical. The query regrouping requires deciding on the ownership pattern (Db hands out `&Connection` vs. domain modules get a trait).

---

### 2. Extract risk scoring into a shared module; eliminate pr_risk/review duplication

**Location:** `src/tools/pr_risk.rs` (161 lines), `src/tools/review.rs` (441 lines)

**What's wrong:** These two tools implement overlapping but divergent risk-scoring logic. Both:
- Call `git::git_diff_files` and `git::git_churn` to get changed paths and churn data
- Iterate changed paths, look up `file.blast_radius` and `symbol.cognitive` from the DB
- Compute normalized scores by dividing raw values by magic-number thresholds (blast/50, cognitive/30, churn/20)
- Combine weighted signals into a composite 0.0-1.0 score

But they diverge in the details: `pr_risk` has four signals (blast 0.35, complexity 0.25, churn 0.20, volume 0.20) while `review` has five (blast 0.30, complexity 0.20, hotspot 0.15, churn 0.15, conventions 0.20). They define separate `ChurnMap` structs (review's lacks `window_days`). They use the same normalization thresholds (50, 30, 20) but there's no shared definition. The `round3` function in review.rs duplicates the `(x * 1000.0).round() / 1000.0` pattern used 8 times in pr_risk.rs. Each tool independently calls `db.file_by_path` and `db.find_symbols_by_file` in a per-path loop.

This is concept spread. The concept of "risk scoring from structural signals" lives in two files that evolve independently. When the scoring model is tuned (inevitable for v1.1 HRR integration), someone will need to find and update both.

**What to do:** Extract a `risk.rs` module (or `scoring.rs`) that owns: the normalization thresholds, a `Signal` enum, a `RiskModel` struct with configurable weights, and a `gather_file_stats(db, paths, churn) -> FileStats` function. Both `pr_risk::compute` and `review::compute` become thin callers. The `ChurnMap` type should be defined once. The `review` module adds its extra signals (hotspot overlap, convention violations) on top of the shared base.

**Effort:** Small (2-3 hours). The extraction is straightforward since both tools already have similar structure.

---

### 3. DD engine is instantiated per-review instead of persisted on the daemon

**Location:** `src/tools/review.rs:119`, `src/dd/engine.rs`

**What's wrong:** The PRD specifies that "DD lives inside the daemon process. Lazy-populated on first DD-backed query. Rebuilt from SQLite facts. Evicted after configurable idle timeout." But the actual implementation in `build_findings` creates a throwaway `DdEngine` on every review call:

```rust
let engine = DdEngine::new(Duration::from_secs(60));
engine.ingest(DdFacts { import_edges: edges })?;
// ... query_forbidden_deps, query_cycles ...
// engine dropped at end of scope
```

This means every `sutra_review` call pays the full DD ingest cost (spawning a timely worker thread, feeding all import edges, stepping the dataflow to completion). For a workspace with thousands of edges, this is significant — and it defeats the purpose of DD's incremental update capability. The `DdEngine::update` method exists but is never called in production code. The `evict_if_idle` method exists but is unreachable because the engine never lives longer than one function call.

The daemon (`src/daemon.rs`) has no reference to `DdEngine`. The `SutraServer` struct has no field for it. There is no integration between DD and the daemon's parse lifecycle.

**What to do:** Add an `Option<DdEngine>` (behind a mutex) to `SutraServer` or `Daemon`. On first DD-backed query, ingest from SQLite (the current code path). On subsequent queries, reuse the warm engine. When the daemon's smriti watcher or scheduler triggers a reparse, call `engine.update(delta)` with the added/removed edges. Wire `evict_if_idle` into the scheduler tick. This is the architecture the PRD describes; it just hasn't been connected yet.

**Effort:** Medium (half-day to a day). The `DdEngine` already has the right API (`ingest`, `update`, `evict_if_idle`). The work is wiring it into the daemon lifecycle and the MCP server's state, and ensuring thread safety between query callers and the update path.

---

### 4. FCA conventions are rebuilt from scratch per-review instead of persisted incrementally

**Location:** `src/tools/review.rs:161-194`, `src/fca/engine.rs`, `src/db.rs:1155-1225`

**What's wrong:** Similar to the DD issue, but worse. The PRD says "FCA extraction runs incrementally on each parse. Results persisted in SQLite." The database schema is ready (`conventions` table with `upsert_convention`, `all_conventions`, `delete_stale_conventions`). The `FcaEngine` has `rebuild` and `update_incremental` methods. But the production code path is:

```rust
// review.rs:build_findings
for f in &all_files {
    let syms = db.find_symbols_by_file(f.id)?;
    for s in &syms {
        if let Some(attrs) = fca::extract_symbol_attrs(&s, &f.path) {
            all_sym_attrs.push(attrs);
        }
    }
}
let mut fca_engine = FcaEngine::new();
fca_engine.rebuild(&all_sym_attrs);
```

Every review call: (1) loads all symbols from all files, (2) extracts attributes for each, (3) does a full FCA rebuild (NextClosure over the entire formal context), (4) checks violations, (5) drops everything. The conventions table in SQLite is populated but never read back during review. The `update_incremental` method is tested but unused in production. The database persistence is write-only infrastructure with no reader.

This is expensive for large workspaces and defeats the incremental design. For sutra's own codebase (1,297 symbols), NextClosure is sub-second. For a 10k-symbol workspace, it could be noticeable.

**What to do:** Move convention extraction into the parse pipeline (`post_parse_sequence`). After resolution and graph computation, extract attributes for newly-parsed files and call `fca_engine.update_incremental`. Persist results to SQLite via `db.upsert_convention`. In `build_findings`, load conventions from SQLite (`db.all_conventions`) instead of rebuilding. This connects the incremental infrastructure that already exists.

**Effort:** Medium (half-day). The pieces all exist; they just need to be wired together. The tricky part is deciding where the `FcaEngine` lives (daemon-level state like DD, or reconstructed from SQLite conventions cheaply since conventions are the output, not the engine state).

---

### 5. Guard subsystem sprawls across three locations with no shared boundary

**Location:** `src/guard.rs` (325 lines), `src/bin/guard.rs` (5,174 lines), `src/main.rs` guard-related code (lines 298-305, 518-656)

**What's wrong:** The guard feature — which intercepts tool calls to warn about high-impact file edits — is implemented across three disconnected locations:

- `src/guard.rs` — library module with `GuardConfig`, `HookInput`, evaluation logic
- `src/bin/guard.rs` — standalone binary (5,174 lines, the single largest file in the codebase) that implements the CLI entry point, JSON parsing, HTTP communication with the sutra daemon, acknowledgment state management, and output formatting
- `src/main.rs` — `cmd_guard_install` and `cmd_guard_uninstall` functions (130 lines) that manipulate Claude Code's `settings.json` to register the guard hooks, plus a `dirs` module and `find_guard_binary` / `claude_settings_path` helpers

The binary at 5,174 lines is an outlier — it's larger than any other single file by a factor of 4x. It likely contains duplicated utility code (config loading, HTTP client setup, workspace resolution) that already exists in the library. The install/uninstall code in `main.rs` is Claude Code-specific integration logic that has no relationship to the server startup code surrounding it.

**What to do:** This is not a v1-blocking issue, but it should be addressed soon after. The guard binary should be thinned by reusing library code. The install/uninstall logic in `main.rs` should move to `src/guard.rs` (or a `guard/install.rs` submodule) so that all guard-related code has a single home. The `dirs` module in `main.rs` should be deleted in favor of the standard approach.

**Effort:** Medium (half-day for the binary thinning, a couple hours for the install logic migration).

---

### 6. Db cache and workspace config are threaded through as raw Arc<Mutex<HashMap>> everywhere

**Location:** `src/mcp.rs:111`, `src/daemon.rs:21`, `src/rest.rs:23`, `src/main.rs:317`, `src/tools/mod.rs:36`, `src/tools/health.rs:13`

**What's wrong:** The pattern `Arc<Mutex<HashMap<String, Arc<Db>>>>` appears in 6 locations as a type. `Arc<RwLock<WorkspacesConfig>>` appears in 5. These are passed as function arguments, stored in structs, cloned for async closures, etc. Both `SutraServer` and `Daemon` contain the same fields (`db_cache`, `workspaces`, `config`) plus `parse_coord` and `scheduler_last_tick`. `main.rs` defines `type DbCache` and `type WsConfig` aliases that are only used locally. The `tools::health::handle` function takes the raw mutex as a parameter rather than a `&Db`.

This is a missing abstraction. The concept of "workspace runtime state" — the workspace registry, the Db cache, the parse coordinator, the scheduler tick — is a coherent unit that gets decomposed into individual fields in every struct that needs it.

**What to do:** Introduce a `WorkspaceRuntime` (or `AppState`) struct that holds the db cache, workspace config, parse coordinator, and optionally the scheduler tick. `SutraServer`, `Daemon`, and `rest::AppState` all take an `Arc<WorkspaceRuntime>` instead of carrying the same four fields independently. `get_or_open_db` becomes a method on `WorkspaceRuntime`. This reduces the parameter-threading ceremony and makes it impossible for the fields to get out of sync between the different holders.

**Effort:** Small-medium (3-4 hours). Mostly mechanical field migration.

---

## Dependency direction issues

No severe inversions found. The dependency flow is clean in the important places:

- `tools/*` -> `db`, `git`, `graph`, `freshness` (downward)
- `review.rs` -> `dd`, `fca`, `git`, `rules` (horizontal composition — review is the only module that knows about both DD and FCA, which is correct per PRD)
- `pipeline.rs` -> `parser`, `resolver`, `graph`, `db` (downward)
- `daemon.rs` -> `pipeline`, `tools`, `workspace`, `smriti` (downward)
- `mcp.rs` -> `tools`, `db`, `config`, `workspace`, `pipeline`, `guard` (top-level orchestration)

One minor concern: `dd/engine.rs` depends on `crate::rules::ForbiddenDep` for the `query_forbidden_deps` signature. This means the DD engine knows about the application's rule format rather than accepting a generic predicate. If DD is ever reused outside sutra's rule system, this would need to be abstracted. Acceptable for v1 given the PRD scope.

---

## Summary

The architecture is sound for a v1 release. The module structure follows the PRD's design. DD and FCA are well-encapsulated modules with clean interfaces. The parser, resolver, and graph subsystems have clear boundaries.

The two most impactful deepening candidates are **3 (DD daemon integration)** and **4 (FCA incremental persistence)**, both of which are cases where the PRD-specified architecture exists in code but isn't wired into the production path. These represent performance and design-intent gaps rather than correctness issues. They should be addressed early in v1.1.

Candidates **1 (Db splitting)** and **2 (risk scoring extraction)** are code health improvements that reduce concept spread and make the codebase easier to evolve. Candidate **6 (WorkspaceRuntime)** is a cleanliness improvement that removes a widespread parameter-threading pattern.

Candidate **5 (guard consolidation)** is the lowest priority but the guard binary's size (5,174 lines) warrants investigation to confirm it isn't carrying significant duplicated code.

None of these are v1-blocking. The codebase is clean, well-tested, and the module boundaries are in the right places.
