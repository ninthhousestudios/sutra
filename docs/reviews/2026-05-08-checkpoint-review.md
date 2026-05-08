# Code Review: sutra @ checkpoint v0.2.0

**Date:** 2026-05-08
**Scope:** All of HEAD (8b0fe30)
**Verdict:** continue with adjustments

No blocking correctness issues. The codebase is functional, well-tested (198 tests, 0 failures), and the architecture is sound for its current scale. The adjustments are: run `cargo fmt` once (49/61 files drifted), and address the duplicate DB query in `compute_pagerank` before the next feature wave. Everything else is follow-up work that the existing refactor plan already covers.

## Verification

- **Build:** Clean. 0 errors, 0 warnings.
- **Tests:** 198 pass, 0 fail, 0 ignored. Good coverage across all layers (db, pipeline, parser, resolver, mcp contracts, REST, daemon watcher, git analysis tools).
- **Clippy:** 14 warnings across 3 categories: too-many-arguments (1), collapsible-ifs (several, concentrated in winnow.rs), complex-type (1). No errors, no unsafe.
- **Format:** 49 of 61 files have format drift. This is not incremental rot -- `cargo fmt` has never been run project-wide. One-time fix.

## Design

The core architecture -- tree-sitter parse into SQLite, symbol/ref/import tables, MCP tool layer on top -- is clean and appropriate for the problem. The daemon/stdio dual-mode design is well-executed: the daemon owns writes and watches smriti for incremental updates, stdio sessions probe the daemon and fall back to local mode. SQLite WAL gives concurrent readers for free.

The main structural tension is that `mcp.rs` (1039 lines) is both the MCP framework integration point AND the home for `sutra_add_root` and `sutra_status`, which contain significant business logic (workspace registration, daemon probing, parse orchestration). The refactor plan's PR 2 (extract `add_root` to `tools/add_root.rs`) addresses this correctly. The other tool handlers in mcp.rs are thin dispatchers (4-8 lines each: resolve workspace, get db, delegate to `tools::*::handle`, wrap response) -- that pattern is sustainable at 20+ tools.

The `Daemon` struct duplicates the same three `Arc<...>` fields as `SutraServer` (`config`, `workspaces`, `db_cache`). This is fine for now -- they serve different roles (daemon = write loop, server = request handler) and share state through the Arcs -- but if a third consumer appears, extract the shared state into a named struct.

`pipeline.rs` at 812 lines with `compute_pagerank` at cognitive complexity 58 is the real hotspot. The function is doing four things: build edge graph, run PageRank iterations, distribute to symbols, and write to DB. The refactor plan's PR 3 (extract graph analytics) is the right fix. The complexity score is high but the logic is straightforward iterative math -- this is a case where the metric overstates the risk.

## Findings

```yaml
- id: F1
  severity: medium
  category: correctness
  title: "compute_pagerank calls all_symbol_file_map() twice"
  location: src/pipeline.rs:646 and src/pipeline.rs:666
  evidence: |
    Line 646: let sym_to_file: HashMap<i64, i64> = db.all_symbol_file_map()?.into_iter().collect();
    (inside the `else` branch when no adjacency provided)
    Line 666: let sym_to_file: HashMap<i64, i64> = db.all_symbol_file_map()?.into_iter().collect();
    (unconditionally, for symbol-level distribution)
  why: |
    When adjacency is None (incremental parse path), the same full-table query
    runs twice. This is a performance waste (two full scans of the symbols table)
    and a subtle correctness concern: in theory the two calls could return
    different results if another thread writes between them, though in practice
    SQLite serialization makes this unlikely.
  recommendation: |
    Hoist the unconditional call at line 666 above the adjacency branch. Use
    it in both the edge-building branch and the symbol distribution section.
    Delete the inner call at line 646.
  confidence: high
```

```yaml
- id: F2
  severity: medium
  category: design
  title: "try_daemon_register silently discards all errors as ()"
  location: src/mcp.rs:920-982
  evidence: |
    Every fallible operation uses .map_err(|_| ()) -- reqwest build, POST send,
    JSON parse, status polling. The caller (sutra_status) treats Err(()) as
    "daemon not available, fall back to local" which is correct for connect
    failures but wrong for e.g. a 500 from the daemon or malformed JSON.
  why: |
    A daemon that is running but returning errors (bad DB, disk full, etc.)
    will be silently ignored and the stdio session will fall back to local mode,
    potentially writing to the same DB the daemon is writing to. The user gets
    no signal that something is wrong.
  recommendation: |
    Return a richer error type (enum with ConnectFailed, DaemonError(status),
    ParseError variants). Fall back to local only on ConnectFailed. For
    DaemonError, return a warning in the sutra_status response:
    {"mode": "local", "daemon_error": "..."}.
  confidence: high
```

```yaml
- id: F3
  severity: low
  category: correctness
  title: "sutra_status swallows add_workspace error on fallback path"
  location: src/mcp.rs:885
  evidence: |
    let _ = workspace::add_workspace(&self.config.workspaces_path, entry.clone());
  why: |
    If writing to workspaces.toml fails (permissions, disk full), the workspace
    is added to the in-memory config but not persisted. Next restart, it
    disappears. sutra_add_root (line 797) correctly propagates this error via
    map_err(sutra_to_rmcp). The fallback path in sutra_status should do the same,
    or at least log the error.
  recommendation: |
    Replace `let _ =` with either `.map_err(sutra_to_rmcp)?` (matching
    sutra_add_root) or at minimum `if let Err(e) = ... { warn!(...) }`.
  confidence: high
```

```yaml
- id: F4
  severity: medium
  category: design
  title: "Daemon uses spawn_blocking + block_on for async parse -- unnecessary indirection"
  location: src/daemon.rs:173-177, 266-270
  evidence: |
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(
            pipeline::parse_changed_files(&ws, &db, &config, &changed, &deleted),
        )
    })
  why: |
    parse_workspace and parse_changed_files are async fns. Wrapping them in
    spawn_blocking + block_on is a pattern for calling sync code from async, not
    for calling async code. This works but wastes a blocking thread pool slot
    and adds indirection. The parse functions are async because they use
    spawn_blocking internally for tree-sitter (CPU-bound), but the outer call
    should just be .await'd directly or spawned with tokio::spawn.
  recommendation: |
    Replace with `tokio::spawn(async move { pipeline::parse_changed_files(...).await })`
    and .await the JoinHandle. This is cleaner and doesn't consume a blocking
    thread to immediately re-enter the async runtime.
  confidence: medium
```

```yaml
- id: F5
  severity: low
  category: contracts
  title: "insert_snapshot takes 9 positional i64 arguments"
  location: src/db.rs:841-852
  evidence: |
    pub fn insert_snapshot(&self, files_parsed: i64, symbols_extracted: i64,
        refs_extracted: i64, parse_errors: i64, duration_ms: i64,
        total_complexity: i64, dead_symbol_count: i64, hotspot_count: i64,
        health_score: i64) -> Result<i64>
  why: |
    Nine positional i64 parameters with no type differentiation. Easy to
    transpose two arguments (e.g. swap parse_errors and duration_ms) and get a
    silent logic bug. Clippy flags this as "too many arguments (10/7)" -- the
    10 count includes &self.
  recommendation: |
    Introduce a `SnapshotParams` struct (or reuse the existing `SnapshotRow`
    minus the auto-generated fields). The refactor plan's PR 4 (split db.rs)
    is a natural place to do this.
  confidence: high
```

## Synthesis

The five findings cluster around two themes:

**Error handling discipline (F2, F3):** The daemon integration path (try_daemon_register, sutra_status fallback) was built for the happy path and treats all failures as "daemon not available." This is the highest-priority fix because it masks real problems during daemon operation. F2 and F3 should be addressed together since they're both in the sutra_status flow.

**Structural complexity in pipeline.rs (F1, F4, F5):** These are all consequences of pipeline.rs and its callers growing organically. F1 (duplicate query) is a quick fix independent of the refactor plan. F4 (spawn_blocking ceremony) is in daemon.rs but directly related to how the pipeline is invoked. F5 (insert_snapshot args) fits naturally into the planned db.rs split (PR 4).

**Suggested fix order:**
1. F1 -- duplicate all_symbol_file_map (5-minute fix, pure win)
2. F3 -- swallowed error in sutra_status (5-minute fix)
3. F2 -- try_daemon_register error types (30-minute fix, pairs with F3)
4. F5 -- insert_snapshot param struct (bundle with refactor PR 4)
5. F4 -- spawn_blocking pattern (bundle with refactor PR 3 or do standalone)

The existing refactor plan is well-sequenced and addresses the right structural issues. None of these findings require changing the plan -- F1-F3 are pre-plan cleanups, F4-F5 fit into planned PRs.

## Slop list

1. **Format drift:** 49/61 files. Run `cargo fmt` once, commit separately. (`src/*.rs`, `src/tools/*.rs`, `src/parser/*.rs`, `src/bin/guard.rs`, `tests/*.rs`)
2. **Collapsible ifs in winnow.rs:** Lines 79-81, 87-89, 94-96, 100-101, 106-108 -- nested `if let` + `if` patterns that clippy wants collapsed. The cognitive complexity of 57 is partly driven by this style.
3. **SKIP_DIRS not used via sutra_dead false positive:** `src/pipeline.rs:34` -- the const IS used at line 486 in `walk_source_files`. sutra_dead flagged it because it can't trace const usage through array containment checks. Not actually dead.
4. **MAX_DEPTH in trace.rs:** `src/tools/trace.rs:9` -- IS used at line 25 (`depth.min(MAX_DEPTH)`). Same sutra_dead false positive pattern. Not dead.
5. **rest.rs add_workspace:** `src/rest.rs:85` -- IS wired into the router at line 34 (`.route("/workspaces", post(add_workspace))`). sutra_dead can't trace axum macro registration. Not dead.
6. **Duplicate SutraServer/Daemon field set:** Both hold `Arc<Config>`, `Arc<RwLock<WorkspacesConfig>>`, `Arc<Mutex<HashMap<String, Arc<Db>>>>`. Not urgent but worth extracting a `SharedState` struct if a third consumer appears.
7. **sutra_add_root and sutra_status duplicate workspace registration logic:** Lines 786-801 vs 876-888 in mcp.rs. The extract-add_root refactor (PR 2) should unify this.
