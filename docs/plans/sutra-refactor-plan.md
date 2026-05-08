# Sutra refactor — implementation plan

Date: 2026-04-30
Source: `docs/sutra-overall-refactor.md`

This plan turns the six candidates into concrete, separately-shippable PRs.
Sequencing follows the source doc; effort estimates assume a single focused
session per PR. Each PR ends green (`cargo test` 165 tests passing) and is
committable on its own.

---

## PR 1 — Merge `tools/grep.rs` into `tools/find.rs`

**Effort:** trivial (~15 min)

**Current state:**
- `tools/grep.rs` (34 lines): `db.find_symbols_by_name`, default limit 20, returns `docstring`.
- `tools/find.rs` (34 lines): same call, default limit 10, returns `visibility`.

**Steps:**
1. In `tools/find.rs::handle`, return both `docstring` and `visibility` in each
   match (no caller is harmed by an extra field).
2. Add a second entry point `tools/find.rs::handle_grep` that just calls
   `handle` with default limit 20. Or take a `default_limit: i64` param.
3. Delete `tools/grep.rs`. Remove `pub mod grep` from `tools/mod.rs`.
4. In `mcp.rs`, change the `sutra_grep` handler to call
   `tools::find::handle_grep(...)` (or `tools::find::handle` with the larger
   default).
5. `cargo test`.

**Verification:** existing tests pass. Both MCP tools (`sutra_find`,
`sutra_grep`) still respond with their expected default limits.

**Open question:** keep `sutra_grep` as a separate MCP tool, or collapse to
just `sutra_find`? **My suggestion:** keep both MCP tools — they're a thin
naming convenience for callers and removing one is a public-interface change
that isn't worth bundling into a cleanup. (See Decision 1 below.)

---

## PR 2 — Extract `sutra_add_root` to `tools/add_root.rs`

**Effort:** trivial (~30 min)

**Current state:** `mcp.rs` lines 534–610 contain ~75 lines of inlined logic
in the router (path validation, config mutation, `parsing_in_progress` guard,
`tokio::spawn`).

**Steps:**
1. Create `src/tools/add_root.rs` with `pub async fn handle(...)` that takes
   the inputs the router currently has access to: `path: &str`, `languages:
   Option<Vec<String>>`, plus refs to the shared state (`workspaces:
   Arc<RwLock<Config>>`, `workspaces_path: &Path`, `parsing_in_progress:
   Arc<Mutex<HashSet<String>>>`, `config: Arc<...>`, and a way to get the DB).
2. Decide the parameter shape — see Decision 2 below.
3. Move the body of `sutra_add_root` into `add_root::handle`. Return a
   `serde_json::Value` (the response payload) so the router can serialize
   uniformly with other tools.
4. The router becomes the same 4-line shape as every other handler:
   resolve → delegate → wrap.
5. Add a unit test for the path-validation branch (absolute + exists check).
6. `cargo test`.

**Verification:** registering a workspace via MCP still works end-to-end;
re-registering still triggers reparse; concurrent add_root for the same id
still hits the "parse already in progress" branch.

---

## PR 3 — Extract graph analytics from `pipeline.rs` into `graph.rs`

**Effort:** small (~1 hr)

**Current state:** `pipeline.rs` (639 lines) mixes parse orchestration with
graph algorithms. Functions to move:
- `build_file_adjacency` (line 389)
- `compute_rollups_with_graph` (line 416)
- `bfs_blast_radius` (line 465)
- `compute_pagerank` (line 503)

**Steps:**
1. Create `src/graph.rs`. Move the four functions. Make those that the
   pipeline calls `pub`; keep `bfs_blast_radius` `pub(crate)` if only used by
   `compute_rollups_with_graph`.
2. Define a small input type: `pub struct ResolvedGraph { pub edges:
   Vec<(i64, i64)>, pub file_symbols: Vec<(i64, i64)> }` — whatever shape
   `parse_workspace` already builds before calling these. Or pass primitives
   if that's what the existing functions take. Don't invent abstractions.
3. In `pipeline.rs::parse_workspace`, after resolution completes, call
   `graph::compute_rollups_with_graph(...)` and `graph::compute_pagerank(...)`.
4. Add `pub mod graph;` to `lib.rs` (will be tightened in PR 5).
5. Move the unit tests for these functions alongside (if any are inline).
6. `cargo test`.

**Verification:** parse output (file rollups, PageRank scores) byte-identical
before and after — diff a fresh `.sutra/<workspace>.db` if needed, or just
trust the test suite.

**Note:** the source doc mentions this "pairs naturally" with PR 4 (db
analytics split) — and it does. After PR 3 lands, `graph.rs` will be the
primary caller of the analytics queries that PR 4 splits out. Land 3 first,
then 4 — the directional dependency is one-way.

---

## PR 4 — Split `db.rs` into core CRUD and analytics

**Effort:** small-medium (~1.5 hr)

**Current state:** `db.rs` (910 lines, 37 pub methods on `Db`). The source
doc lists ~12 analytics methods to split out. Concretely from the grep
output:
- `find_dead_symbols` (538)
- `find_unreachable_files` (599)
- `dead_symbol_ratio_by_file` (573)
- `complexity_by_file` (522)
- `batch_update_file_pagerank` (263)
- `batch_update_symbol_pagerank` (278)
- `all_symbol_file_map` (787)
- `all_resolved_refs` (797)
- `symbol_counts_by_file` (510)
- `import_edges` (775) — used by graph; debatable whether this is "analytics" or core CRUD. **My suggestion: leave in core**, since it's a straight read of the imports table with no aggregation.
- `find_files_referencing_symbols` (715) — same call.

**Steps (assuming a sub-module split, see Decision 3):**
1. Create `src/db/` directory. Move `db.rs` → `db/mod.rs` (keep types and the
   `Db` struct here; keep `open`, `workspace_id`, schema migrations).
2. Create `src/db/analytics.rs` with `impl Db { ... }` containing the moved
   methods. Use `use super::*;` for shared types.
3. The `Db` struct stays in `mod.rs`; `analytics.rs` only adds methods. No
   visibility changes needed.
4. `cargo test`.

**Verification:** every existing call site still compiles unchanged
(`db.find_dead_symbols(...)` still resolves).

---

## PR 5 — Curate `lib.rs` as a real API surface

**Effort:** small (~45 min)

**Current state:** `lib.rs` is 12 lines of `pub mod`. Every internal module
is reachable from any consumer.

**Steps:**
1. Audit consumers:
   - `src/bin/guard.rs` — what does it import from the crate?
   - `src/main.rs` — daemon entry. Probably touches `daemon`, `mcp`, `config`.
   - `tests/` — integration tests; what do they import?
2. Demote modules to `pub(crate)` where no external consumer needs them.
   Likely candidates: `pipeline`, `parser`, `resolver`, `daemon`, `mcp`.
3. Keep `pub` on what binaries and tests genuinely use. Add `pub use`
   re-exports for the few types that need to cross the boundary (e.g.
   `Config`, `Db`, `guard::*`, `error::Result`).
4. `cargo build --bins --tests` to flush out every visibility break.
5. Fix the breaks by either (a) adding a targeted `pub use` or (b) moving the
   caller to use a re-exported type.
6. `cargo test`.

**Verification:** all bins and tests compile. The diff of `lib.rs` should
show a meaningfully smaller surface than before.

**Risk:** test files may reach into internals (e.g. `crate::pipeline::*`).
Each one is a small fix — an integration test that needs deep internals
should probably be a unit test inside the module instead. Note this if you
find one but don't refactor in this PR.

---

## PR 6 — Resolution interface (resolver trait)

**Effort:** medium (~3 hr). Defer until #1–#5 land.

**Current state:** `pipeline.rs::resolve_file_refs` (lines 177–256) re-inflates
DB rows into `parser::ExtractedSymbol` / `ExtractedRef` / `ExtractedImport`
just to call `resolver::resolve_refs`, then writes back. This is ~50 lines of
conversion ceremony per file resolved.

**Two options from the source doc:**

- **(a) Shared resolution-ready type.** Introduce e.g. `ResolverSymbol`
  somewhere neutral (a new `resolver_types.rs`?), used by both parser and DB.
  Eliminates the conversion but couples the parser's output type to the DB's
  shape. Lower leverage.

- **(b) Trait-based lookup.** `resolver::resolve_refs` takes a trait like
  `SymbolLookup` instead of `&[(i64, String, String, String)]`. Pipeline
  implements it over DB queries. Higher leverage — resolver becomes streaming-
  friendly — but more design work. **My suggestion: do (b)**, since the source
  doc explicitly favors it for memory leverage on large workspaces, and (a)
  just smears the same coupling across more types. (See Decision 4.)

**Steps for (b):**
1. Define `pub trait SymbolLookup { fn lookup(&self, name: &str) -> Vec<ResolvedSymbolRef>; }` (or whatever method shape the resolver actually needs — read `resolver.rs` first to see what `all_symbols` is consulted for).
2. Make `resolver::resolve_refs` generic over `T: SymbolLookup` instead of taking the materialized slice.
3. Implement `SymbolLookup` for a `DbLookup<'a>(&'a Db)` adapter in `pipeline.rs` (or `resolver_adapters.rs`).
4. Delete `extracted_symbols`/`extracted_refs`/`extracted_imports` re-inflation in `resolve_file_refs`. The body shrinks substantially.
5. Keep a `Vec`-backed `SymbolLookup` impl for tests, so resolver unit tests don't need a DB.
6. `cargo test`.

**Verification:** resolution output identical for the test suite. Optional:
benchmark on the manas workspace — should be no slower (DB queries already
happen, just funneled through the trait).

**Defer signal:** if test coverage on the resolver is thin, expand it before
this PR — without good resolver tests, the trait change is risky.

---

## Suggested commit cadence

One PR per session. PRs 1–2 can land same session if you're feeling brisk.
PR 5 should land before any new external consumer of the library is added,
otherwise the `pub` audit gets harder.

Total estimate: **~7 hours of focused work**, spread across 4–6 sessions.

---

## Design decisions I need from you

### Decision 1 — Keep both `sutra_grep` and `sutra_find` MCP tools?

Both are thin. They differ only in default limit and one returned field. The
refactor doc suggests "merge into one tool handler that returns both fields,
or make `grep` a thin alias over `find` with different defaults" — both keep
two MCP tools but unify the implementation.

**Suggestion:** unify the implementation behind `tools::find::handle`, but
keep `sutra_grep` and `sutra_find` as separate MCP tools. Removing an MCP
tool is a public-interface break for any agent that already calls
`sutra_grep`. Code-level merge gives all the locality benefit; surface-level
merge gives almost none and breaks callers.

### Decision 2 — Where does `add_root` get its shared state?

The router's `sutra_add_root` reaches into `self.workspaces`,
`self.parsing_in_progress`, `self.config`, and `self.get_db(...)`. The
extracted `tools/add_root::handle` needs that state. Options:

- **(a)** Pass each piece as a parameter (5–6 args). Most explicit, ugliest signature.
- **(b)** Define a small `AddRootCtx<'a>` struct in `tools/add_root.rs` that
  bundles the refs. Clean signature, one extra type.
- **(c)** Move `handle` to a method on `&SutraServer` defined in
  `tools/add_root.rs` via `impl SutraServer`. Lets it call `self.get_db`
  unchanged. Couples the tool module to the server type.

**Suggestion: (b).** It matches how other tool handlers take `&Db` as a
focused parameter rather than the whole server. Single call site, so the
extra type costs nothing.

### Decision 3 — `db/analytics.rs` as a sub-module, or `db_analytics.rs` as a sibling?

The source doc says either is fine. Both share the same `Db` struct.

- **Sub-module (`db/analytics.rs`):** keeps `db` as a logical unit; both
  files can `use super::*;` for shared row types. Requires turning `db.rs`
  into `db/mod.rs`.
- **Sibling (`db_analytics.rs`):** simpler diff — no file rename — but
  visually pretends `db` and `db_analytics` are peer concepts when really one
  is parasitic on the other.

**Suggestion: sub-module.** The sub-module form makes the relationship clear
and matches how `parser/` is already organized (`parser/rust.rs`,
`parser/dart.rs`, `parser/complexity.rs`).

### Decision 4 — Resolver interface: type-based (a) or trait-based (b)?

Covered in PR 6. **Suggestion: trait (b)**, but only when you're ready to
spend the design budget. There is no urgency until workspace size becomes a
problem. If you want the smallest possible change in PR 6, do (a) instead and
revisit later.

### Decision 5 — Should the graph module own the analytics DB queries it consumes?

Once PR 3 and PR 4 both land, `graph.rs` will be the dominant consumer of
the new `db::analytics` methods (`all_symbol_file_map`, `all_resolved_refs`,
`batch_update_file_pagerank`, etc.). One could argue the graph module should
own those queries directly (graph as a "deep module" that hides the SQL),
making `db::analytics` a private detail.

**Suggestion: don't.** Keep DB access in `db::analytics`. The graph module
should stay algorithm-only — easier to test (pass in adjacency, get scores
back) and reusable by future analysis tools that are not the graph. This is
the seam the source doc describes ("here's an adjacency set, give me
scores"). Making graph own the queries would re-couple algorithm to storage.

---

## What this plan deliberately does not do

- No new features. The qartez survey items (`context`, `test_gaps`, `clones`)
  are listed as motivation but not scoped here.
- No test additions beyond what the refactors directly enable (e.g. resolver
  unit tests in PR 6). If you want broader test work, that's a separate doc.
- No SQL schema changes. Every PR is pure code reorganization except PR 6,
  which changes a function signature.
- No `parser/*` reorganization. The parser tree is healthy.
