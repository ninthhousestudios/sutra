# Architecture Pass — sutra v0.2.0 Checkpoint

Date: 2026-05-08
Scope: Deepening candidates from structural review of 61 files, 676 symbols.
Status: Candidates only (pre-grilling).

---

## 1. Extract a pipeline orchestration module from `pipeline.rs`

**Files:** `src/pipeline.rs` (812 lines, max cognitive 58)

**Problem:** `pipeline.rs` contains at least four distinct responsibilities packed into one file: file walking/discovery, single-file parse-and-persist, reference resolution, and graph analysis (rollups, PageRank, snapshot aggregation). The module's interface is just two public functions (`parse_workspace`, `parse_changed_files`), but internally these are 120-line monoliths that sequence six phases each. Worse, `parse_workspace` and `parse_changed_files` duplicate the post-parse orchestration (phases 3-5: cascade resolution, rollup computation, PageRank, snapshot insertion) almost line-for-line. The `compute_pagerank` function alone is 122 lines with cognitive complexity 58 — the highest in the codebase.

**Solution:** Split pipeline into at least two modules: one for per-file parsing and reference resolution (the mechanical work), one for graph analysis (rollups, PageRank, snapshot aggregation). Extract the shared post-parse orchestration sequence (resolve dirty refs, build adjacency, compute rollups, compute PageRank, record snapshot) into a single function that both `parse_workspace` and `parse_changed_files` call. The `compute_pagerank` function deserves to be its own module behind a small interface: `fn compute(db: &Db, files: &[FileRow]) -> Result<()>`.

**Benefits:**
- **Locality:** PageRank bugs, rollup bugs, and parse bugs stop being tangled in a single 812-line file. Changes to the ranking algorithm don't require understanding file-walking code.
- **Leverage:** The shared post-parse orchestration gives callers (full parse, incremental parse, and any future partial-reparse path) one sequence to maintain instead of two diverging copies.
- **Testability:** PageRank and rollup computation can be tested in isolation with synthetic graphs, without needing to set up workspace directories and run the full parse pipeline.


## 2. Separate MCP dispatch shell from tool-handler logic in `mcp.rs`

**Files:** `src/mcp.rs` (1039 lines, 66 symbols), `src/tools/*.rs` (20 tool modules)

**Problem:** `mcp.rs` is the largest file in the codebase but is architecturally shallow. Each tool handler method follows an identical pattern: `resolve_workspace` / `get_db` / optionally `require_analysis` / call into `tools::*::handle(...)` / `wrap_response`. The 24 arg structs (lines 26-252) exist solely to satisfy rmcp's derive macros. The real tool logic already lives in `src/tools/*.rs`. The module's interface (the `#[tool]` macro registrations) is enormous — 66 symbols — but the implementation behind each is just 5-10 lines of glue. Apply the deletion test: if you deleted `mcp.rs`, the tools themselves would survive intact; the only thing lost is the MCP-framework wiring and the arg struct declarations.

This is a classic pass-through pattern. Every new tool requires touching `mcp.rs` to add an arg struct and a handler, even though the actual behavior lives elsewhere. The `src/tools/mod.rs` churn signal (7 commits) confirms this: each new tool forces coordinated edits across `mcp.rs` and `tools/mod.rs`.

**Solution:** Move each tool's arg struct into its corresponding `src/tools/*.rs` module (co-locate interface with implementation). Investigate whether rmcp's `#[tool]` macro can be applied directly to functions in the tool modules, or if a thin registration layer can auto-discover tools. The goal is that adding a new tool requires editing exactly one file.

**Benefits:**
- **Locality:** A new tool is one file, one commit. No coordinated edit across `mcp.rs` + `tools/foo.rs` + `tools/mod.rs`.
- **Leverage:** The remaining MCP shell becomes truly thin — just server lifecycle, workspace resolution, and response wrapping. Callers (the MCP framework) get the same interface; maintainers touch fewer files.
- **Testability:** No change (tools are already independently testable), but the reduced coordination means less merge friction.


## 3. Extract a `graph` module for file-level dependency graph operations

**Files:** `src/pipeline.rs` (functions: `build_file_adjacency`, `compute_rollups_with_graph`, `bfs_blast_radius`, `compute_pagerank`), `src/db.rs` (methods: `all_symbol_file_map`, `all_resolved_refs`, `import_edges`, `batch_update_file_pagerank`, `batch_update_symbol_pagerank`, `update_rollups`)

**Problem:** The file-level dependency graph is a coherent concept with its own data structures (`FileGraph` type alias, adjacency maps, BFS state), algorithms (PageRank iteration, BFS blast radius, fan-in/fan-out rollups), and persistence (batch updates to `pagerank` and `rollup` columns). Currently these are split between `pipeline.rs` (algorithms) and `db.rs` (persistence), with no explicit seam between them. The graph operations depend on `Db` for data loading and result storage, but `Db` doesn't know about graphs — it just provides raw query methods. This means `db.rs` accumulates graph-specific query methods (churn=18, highest in codebase) that have nothing to do with its core responsibility of schema/migration/CRUD.

**Solution:** Introduce a `graph` module that owns the `FileGraph` type, the adjacency builder, BFS, PageRank, and rollup computation. It takes `&Db` as a dependency for data access. The graph module's interface would be two functions: `compute_rollups(db, files, changed_ids)` and `compute_pagerank(db, files)`. This pulls ~250 lines out of `pipeline.rs` and gives the graph concept a home.

**Benefits:**
- **Locality:** All graph algorithms in one place. A bug in PageRank iteration or BFS is found and fixed in one module, not scattered across pipeline orchestration code.
- **Leverage:** Pipeline callers just call `graph::compute(...)` — they don't need to understand adjacency construction, warm-start heuristics, or symbol-level PageRank distribution.
- **Testability:** Graph algorithms can be tested with synthetic adjacency data without going through the full parse pipeline. Currently the only way to test PageRank is `tests/pagerank_test.rs` which must set up a real DB and parse files.


## 4. Deepen `db.rs` by hiding query implementation behind domain methods

**Files:** `src/db.rs` (998 lines, 52 symbols, churn=18, blast=11)

**Problem:** `Db` exposes 45+ public methods, most of which are thin wrappers around a single SQL query. The interface is nearly as complex as the implementation — callers must know which specific query method to call, in what order, and how to combine results. For example, computing "files that need re-resolution after symbol deletion" requires calling `find_files_referencing_symbols` then iterating, which is done identically in both `parse_workspace` and `parse_changed_files`. The five `map_*_row` functions at the bottom are pure boilerplate that repeat column-index extraction.

The module is shallow: 52 symbols, but almost all are leaf query methods with no domain intelligence. The high churn (18) follows from this — every new feature that needs data adds another query method, growing the interface linearly.

**Solution:** Group related queries behind fewer domain-level methods. For example, `dirty_files_after_deletion(symbol_ids) -> HashSet<i64>` instead of exposing `find_files_referencing_symbols` and letting callers loop. Consider whether the row-mapping functions can use a derive macro or a generic mapper to eliminate the ~80 lines of column-index boilerplate. The goal is not to remove query methods but to offer higher-level operations that reduce the caller's cognitive load.

**Benefits:**
- **Leverage:** Callers get domain operations ("which files are dirty?") instead of SQL primitives ("find files referencing these symbol IDs"). The interface shrinks conceptually even if the method count doesn't change dramatically.
- **Locality:** Domain logic about what constitutes "dirty" or "dead" or "unreachable" lives in one place instead of being re-derived at each call site.
- **Testability:** Domain methods can enforce invariants (e.g., always re-resolve after cascade delete) that raw query methods cannot.


## 5. Consolidate workspace registration logic between `sutra_add_root` and `sutra_status`

**Files:** `src/mcp.rs` (methods `sutra_add_root` lines 766-842, `sutra_status` lines 848-916)

**Problem:** These two MCP handlers duplicate workspace registration logic: path validation, workspace-id derivation from directory name, language defaulting, workspace config insertion, and conditional parsing. They diverge only in that `sutra_status` tries the daemon first and returns status fields, while `sutra_add_root` always parses locally and spawns a background task. The workspace-id derivation (`dir_name.to_lowercase().replace(' ', "-")`) is repeated verbatim. Both are the highest-complexity methods in `mcp.rs` (cognitive 7 each). This is the kind of duplication that breeds subtle divergence over time — one gets a fix, the other doesn't.

**Solution:** Extract a `register_workspace(root, languages) -> (WorkspaceEntry, bool)` method on `SutraServer` that handles validation, id derivation, and config insertion. Both `sutra_add_root` and `sutra_status` become thin wrappers that call this shared method and then diverge only on their specific behavior (background parse vs. daemon probe).

**Benefits:**
- **Locality:** Workspace registration rules live in one method. A change to id-derivation logic or default languages is a single edit.
- **Leverage:** Future workspace-related tools (e.g., `sutra_remove_root`) can reuse the same registration infrastructure.
- **Testability:** The registration method can be tested independently of MCP framework plumbing.


## 6. Make `winnow.rs` compose existing tool primitives instead of reimplementing them

**Files:** `src/tools/winnow.rs` (166 lines, cognitive complexity 57, health score 54)

**Problem:** Winnow is a multi-axis symbol filter/ranker, but it reimplements functionality that already exists in other tool modules: file iteration and freshness checking (duplicated from `map.rs`), churn lookup (duplicated from `hotspots.rs`), symbol lookup by caller (duplicated from `calls.rs`). The single `handle` function has cognitive complexity 57 — second highest in the codebase — because it inlines all filter axes (kind, complexity, churn, glob, regex, calls-to) into one nested loop with six conditional `continue` branches.

Apply the deletion test: if winnow were deleted, its callers would lose the ability to combine these filters in a single call, but each individual filter axis exists elsewhere. Winnow's value is the combination, not any individual piece.

**Solution:** Factor each filter axis into a predicate (a closure or small struct implementing a filter trait). The main function becomes: load symbols, apply predicate chain, sort, truncate, format. Each predicate can be tested independently. The churn and caller-lookup logic should call into the existing `git::git_churn` and calls infrastructure rather than reimplementing the queries inline.

**Benefits:**
- **Locality:** Each filter axis is isolated. Adding a new axis (e.g., "symbols with stale freshness") means adding one predicate, not weaving another conditional into the nested loop.
- **Leverage:** The predicate-chain pattern can be reused if other tools need filtered symbol iteration.
- **Testability:** Individual predicates can be unit-tested. Currently the only way to test winnow is end-to-end through the MCP contract tests.
