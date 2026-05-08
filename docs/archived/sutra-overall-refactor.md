# Sutra — architecture deepening opportunities

Date: 2026-04-29
Method: organic codebase exploration using the improve-codebase-architecture
skill vocabulary (module, interface, depth, seam, adapter, leverage, locality).

## Codebase shape

| Module | Lines | Symbols | Role |
|---|---|---|---|
| `db.rs` | 910 | 48 | SQLite operations — CRUD + analytics queries |
| `mcp.rs` | 657 | 51 | MCP router — thin dispatcher to `tools/*` |
| `pipeline.rs` | 639 | 16 | Parse orchestration + graph analytics |
| `parser/rust.rs` | 633 | 23 | Tree-sitter Rust extraction |
| `parser/dart.rs` | 475 | 19 | Tree-sitter Dart extraction |
| `tools/*` (16 files) | ~800 | 18 | Tool handlers, 1 per file |
| Everything else | ~1500 | ~80 | Wiring, config, guard, resolver |

47 files total, 165 tests passing. Overall design is sound — clean
separation between parsing (stateless), resolution (pure function), DB
(single deep module), and MCP routing (thin glue). What follows are places
where the current structure creates friction or fails to earn its depth.

---

## Candidates

### 1. Extract graph analytics from `pipeline.rs`

**Files:** `src/pipeline.rs` (639 lines)

**Problem:** `pipeline.rs` does two distinct jobs: (a) the parse-then-resolve
pipeline (filesystem walk, content-hash skip, parse dispatch, DB writes,
reference resolution) and (b) graph analytics computed over the resolved graph
(`build_file_adjacency`, `compute_rollups_with_graph`, `bfs_blast_radius`,
`compute_pagerank`). Different concerns, different change reasons. A bug in
PageRank iteration and a bug in content-hash skipping land in the same
639-line file. As the roadmap adds more analytics (`context`, `test_gaps`,
`clones` — all per the qartez feature survey), the graph analytics half will
keep growing while the parse orchestration stays stable.

**Solution:** Extract graph analytics into a `graph.rs` (or `analytics.rs`)
module. The seam is natural: after resolution completes, pipeline hands the
resolved ref graph to the graph module, which returns computed scores.
Pipeline writes them back to DB.

**Benefits:** Locality — graph algorithm bugs and new graph metrics
concentrate in one place. Leverage — the graph module's interface ("here's an
adjacency set, give me scores") is reusable by future tools that need graph
traversal without triggering a full reparse. Tests for graph algorithms
wouldn't need parse fixtures.

**Estimated effort:** Small. The functions are already self-contained; this is
a file-level move plus adjusting the call in `parse_workspace`.

---

### 2. Split `db.rs` analytics queries from CRUD

**Files:** `src/db.rs` (910 lines, 37 pub methods)

**Problem:** `db.rs` is currently a deep module — 37 methods behind a single
`Db` struct, well-grouped by concern. But it's doing double duty: core entity
operations (file/symbol/ref/import CRUD, ~25 methods) and analytics queries
(`find_dead_symbols`, `find_unreachable_files`, `dead_symbol_ratio_by_file`,
`complexity_by_file`, `batch_update_file_pagerank`,
`batch_update_symbol_pagerank`, `all_symbol_file_map`, `all_resolved_refs`,
~12 methods). The analytics queries serve the graph computations and analysis
tools, not the core parse pipeline. As more analysis tools land, this second
responsibility will keep growing.

**Solution:** Split `Db` methods into core (`db.rs`) and analytics
(`db/analytics.rs`), either as separate impl blocks in separate files or as a
sub-module. Both share the same `Db` struct and connection — the split is
organizational, not architectural.

**Benefits:** Locality — adding a new analytics query doesn't require reading
past 25 CRUD methods. Leverage — the CRUD interface stabilizes faster when it
isn't growing alongside analytics. Deletion test: removing the analytics
methods leaves the parse pipeline and core tools fully functional; complexity
reappears only in the analysis tools that actually need those queries.

**Estimated effort:** Small-medium. Requires splitting one large impl block
into two files with shared struct visibility.

---

### 3. Merge `tools/grep.rs` and `tools/find.rs`

**Files:** `src/tools/grep.rs` (34 lines), `src/tools/find.rs` (34 lines)

**Problem:** Both call `db.find_symbols_by_name` with slightly different
default limits (20 vs 10) and slightly different output fields (`docstring`
vs `visibility`). Clearest case of a module that fails the deletion test —
deleting `grep.rs` would lose nothing that isn't recoverable as a one-line
change to `find.rs`. Two files with nearly identical implementations behind
nearly identical interfaces produce no leverage and scatter a single concept
across two locations.

**Solution:** Merge into one tool handler that returns both fields, or make
`grep` a thin alias over `find` with different defaults.

**Benefits:** Locality — one place to understand symbol search. Eliminates the
question "when do I use grep vs find?" for callers.

**Estimated effort:** Trivial.

---

### 4. Introduce a resolution interface to eliminate the conversion ceremony

**Files:** `src/pipeline.rs` (`resolve_file_refs`), `src/resolver.rs`

**Problem:** The resolver is a pure function that speaks parser types
(`ExtractedSymbol`, `ExtractedRef`). But at resolution time, the data lives
in DB rows. So `pipeline.rs::resolve_file_refs` retrieves rows from the DB,
manually re-inflates them into `ExtractedSymbol`/`ExtractedRef`, calls the
resolver, then writes the results back. This conversion layer — DB rows to
parser types to resolved refs to DB writes — is the tax for the resolver's
purity. The resolver doesn't know about the DB, which is good. But the
conversion means any change to `ExtractedSymbol`'s shape requires coordinated
edits in the parser, the pipeline, and the resolver.

**Solution:** Two options: (a) introduce a shared "resolution-ready" type
distinct from both `ExtractedSymbol` and `SymbolRow` that the pipeline
constructs and the resolver consumes, or (b) have the resolver accept a trait
for symbol lookup rather than a pre-materialized list — the pipeline could
implement that trait over DB queries, eliminating the re-inflation entirely.

**Benefits:** Leverage — the resolver becomes usable in contexts where
materializing all symbols into memory isn't desirable (e.g., large
workspaces). Locality — the conversion logic that currently sprawls across
`resolve_file_refs` concentrates behind the trait implementation.

**Estimated effort:** Medium. Option (b) is more work but higher leverage.

---

### 5. Curate `lib.rs` as a real API surface

**Files:** `src/lib.rs` (12 lines — just `pub mod` declarations)

**Problem:** `lib.rs` exposes every internal module to every consumer. The
guard binary (`src/bin/guard.rs`) only needs `guard::*` and read-only DB
access, but it can reach `pipeline`, `mcp`, `daemon` — modules it has no
business touching. A flat `pub mod` surface means the library's interface is
as complex as its implementation — depth zero.

**Solution:** Curate `lib.rs` with selective `pub use` re-exports. Make
internal modules `pub(crate)` where they're only used within the crate. The
guard binary's needs are small — expose a `guard` facade. The daemon/MCP
wiring modules can be `pub(crate)`.

**Benefits:** Depth — the library's interface shrinks to what external
consumers actually need. Leverage — new consumers (future binaries,
integration tests) get a clear API instead of the full internals. Makes it
impossible to accidentally couple the guard to the parse pipeline.

**Estimated effort:** Small, but requires auditing what `bin/guard.rs` and
tests actually import.

---

### 6. Extract `sutra_add_root` from `mcp.rs`

**Files:** `src/mcp.rs` (lines ~534-610)

**Problem:** Every other tool handler in `mcp.rs` follows a clean 4-line
pattern: resolve workspace, get DB, delegate to `tools::X::handle`, wrap
response. `sutra_add_root` breaks this — it has ~75 lines of inlined logic
(path validation, config mutation, parse-in-progress guard, `tokio::spawn`)
directly in the router. This reduces locality: if you want to understand
"what happens when a root is added," you look in the router, not in the
tools layer where every other tool lives.

**Solution:** Extract the body into `tools/add_root.rs`, matching the pattern
of every other handler.

**Benefits:** Locality — root-addition logic concentrates in the tools layer.
The router stays a pure dispatcher. Easy to test the add-root logic
independently.

**Estimated effort:** Trivial.

---

## Suggested sequencing

1. **Quick wins first:** #3 (merge grep/find) and #6 (extract add_root) —
   trivial effort, immediate clarity.
2. **Graph extraction:** #1 (extract graph analytics from pipeline.rs) —
   small effort, clears the path for new analytics features on the roadmap.
3. **DB split:** #2 (split db.rs analytics from CRUD) — pairs naturally with
   #1 since the graph module will be the primary consumer of analytics
   queries.
4. **API surface:** #5 (curate lib.rs) — small effort, prevents future
   coupling mistakes.
5. **Resolver interface:** #4 (resolution trait) — medium effort, highest
   leverage long-term, but not urgent until workspace sizes push memory
   limits.

Total: ~5 focused PRs across multiple sessions.
