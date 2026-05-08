# Sutra Code Review — GLM-5.1 Initial Review

**Date:** 2026-04-27
**Reviewer:** opencode/glm-5.1
**Scope:** Full codebase, all `src/` files, tests, migrations, Cargo.toml

---

## 1. Executive Summary

Sutra is a code-intelligence MCP server built in Rust. It parses source files via tree-sitter, stores symbols/refs/imports in SQLite with FTS5, and exposes 14 MCP tools for code navigation, impact analysis, and git-aware co-change tracking. The codebase is ~38K LOC across 171 source files (excluding build artifacts), with 5,983 indexed symbols.

**Overall quality: Good for a v0.1.** The architecture is clean, the error model is well-designed, and the database schema is sensible. The main concerns are: two god functions that dominate the cyclomatic complexity budget, significant code duplication in tool handlers and DB query methods, N+1 query patterns in performance-critical paths, and thin test coverage.

| Category | Rating | Summary |
|----------|--------|---------|
| Architecture | B+ | Clean layering, good separation of concerns |
| Correctness | B | Resolver is approximate by design; some edge-case bugs |
| Performance | C+ | N+1 queries in hot paths; single-threaded DB access |
| Test Coverage | D+ | 62% of source files have zero test coverage |
| Security | A- | No app-level issues; all findings are from bindgen artifacts |
| Maintainability | B- | Two god functions, duplicated patterns across tools |

---

## 2. Architecture Overview

```
main.rs ── CLI (clap)
  ├── Serve (stdio/HTTP) ── mcp.rs (SutraServer, 14 tools)
  ├── Parse ─────────────── pipeline.rs ── parser/* ── resolver.rs ── db.rs
  └── Workspaces ────────── workspace.rs

daemon.rs ──── periodic stale-check + auto-reparse
git.rs ──────── git diff / cochange (shell out to `git`)
smriti_client.rs ── stub (unused)
tools/ ──────── 12 handler modules, one per MCP tool
```

**Strengths:**
- Clean module boundaries. Each tool handler is a pure function `fn handle(db, ...) -> Result<Value>`.
- The `SutraServer` struct in `mcp.rs` composes these handlers via the `rmcp` framework with minimal glue.
- Error handling is systematic: `SutraError` → `ErrorData` → MCP `ErrorData` with actionable `next_action` strings.

**Weaknesses:**
- `pipeline.rs` and `parser/rust.rs` each have a single function that does everything (see §4).
- The `smriti_client.rs` module is a dead stub with no callers.

---

## 3. Detailed Findings

### 3.1 Critical — God Functions

#### `parse_workspace` (`src/pipeline.rs:45-325`, CC=22, 281 lines)

This single `async fn` performs six distinct steps: file walking, per-file parsing, cross-file dirty marking, ref resolution, rollup computation, and snapshot recording. Each step has its own error handling, data flow, and side effects.

**Impact:** Any bug in any step requires reasoning about all 281 lines. The CC=22 makes exhaustive testing infeasible.

**Recommendation:** Extract each step into its own function:
- `walk_and_parse_files()` → `mark_dirty_files()` → `resolve_all_refs()` → `compute_rollups()` → `record_snapshot()`
This alone should drop the parent CC to ~5.

#### `collect_symbols` (`src/parser/rust.rs:50-149`, CC=28, 100 lines)

A single recursive function with a 12-arm `match` on `child.kind()`, each arm doing symbol extraction + optional recursion into child nodes. The same pattern repeats in `src/parser/dart.rs:45-138` (CC=21).

**Recommendation:** Extract per-kind handlers (e.g., `handle_function_item`, `handle_struct_item`, etc.) and use a dispatch table or `match` that delegates to them. Each handler returns `Option<Vec<ExtractedSymbol>>`.

### 3.2 High — N+1 Query Patterns

The DB layer (`src/db.rs`) uses `parking_lot::Mutex<Connection>` with per-row operations. Several tools and the pipeline iterate over file lists and issue individual queries per file:

| Call site | Pattern | Count |
|-----------|---------|-------|
| `pipeline.rs:368-378` `load_all_symbols` | N queries (symbols per file) | O(files) |
| `pipeline.rs:406-418` `compute_rollups` | 2N queries (symbols + refs per file) | O(files) |
| `tools/health.rs:23-26` | N queries (symbols per file for count) | O(files) |
| `tools/map.rs:17` | N queries (symbols per file for count) | O(files) |
| `tools/impact.rs:43` | N queries (refs per symbol in BFS) | O(symbols) |
| `tools/calls.rs:64` | N queries (refs per symbol in BFS) | O(symbols) |

**Recommendation:** Add batch query methods to `Db`:
- `all_symbols_with_file_id() → Vec<(i64, String, String, i64)>` (one query, replaces `load_all_symbols`)
- `symbol_counts_by_file() → HashMap<i64, usize>` (one query, replaces per-file counting)
- `refs_to_symbols(symbol_ids: &[i64]) → Vec<RefRow>` (one query with IN clause, replaces per-symbol BFS queries)

### 3.3 High — Code Duplication

#### Symbol lookup boilerplate (4 files)

The pattern "try `symbol_by_qualified_name`, then fall back to `find_symbols_by_name`" is copy-pasted in `tools/refs.rs:9-22`, `tools/calls.rs:16-29`, `tools/impact.rs:11-24`, and `tools/read.rs:16-29`. The code is identical in each.

**Recommendation:** Extract a helper on `Db`:
```rust
pub fn resolve_symbol(&self, name: &str, kind: Option<&str>) -> Result<Option<SymbolRow>>
```

#### `find.rs` vs `grep.rs` (Clone Group #13)

`tools/find.rs` and `tools/grep.rs` are nearly identical (29-line `handle` functions, same structure). The only differences: `find` defaults `limit=10` and omits `docstring`; `grep` defaults `limit=20` and includes `docstring`.

**Recommendation:** Merge into a single `search.rs` handler with a `mode` parameter, or extract the shared mapping logic into a `symbol_to_json` helper.

#### `find_enclosing_symbol` (2 files)

Duplicated verbatim between `tools/impact.rs:103-122` and `tools/calls.rs:152-171`.

**Recommendation:** Move to `db.rs` or a shared `tools/utils.rs`.

#### DB query boilerplate (Clone Group #5)

`find_symbols_by_file`, `find_refs_to_symbol`, `find_refs_in_file`, `imports_for_file` all follow the same pattern: lock → prepare → query_map → collect. The SQL column list for `SymbolRow` is repeated 7 times across `db.rs`.

**Recommendation:** Define a `SYMBOL_COLUMNS` constant and use a macro or helper for the common `lock → prepare → query_map` pattern.

#### Analysis-tier guard (4 tools)

The `if !self.analysis_enabled.load(...) { return Err(...) }` block is copy-pasted across `sutra_refs`, `sutra_calls`, `sutra_diff_impact`, `sutra_cochange` in `mcp.rs`.

**Recommendation:** Extract a `require_analysis(&self) -> Result<(), ErrorData>` method.

### 3.4 Medium — Correctness Issues

#### Resolver ambiguity resolution (`src/resolver.rs:105-113`)

When multiple global matches exist for a symbol name and the import filter also fails, the resolver just picks `global_matches[0]` — the first result from the DB, which is ordered by insertion order. This is nondeterministic across parse runs and may resolve to the wrong symbol.

**Recommendation:** At minimum, sort by relevance (e.g., prefer symbols in files that share import paths with the referencing file). Add a warning in the output when a disambiguation was made heuristically.

#### `_config` parameter ignored (`src/pipeline.rs:48`)

`parse_workspace` takes `_config: &Config` but never uses it. The parallelism config `parse_parallelism` is never applied — all files are processed sequentially despite the comment "v0.1" noting this limitation.

**Recommendation:** Either remove the parameter or implement the parallelism (using `stream::iter(...).for_each_concurrent(config.parse_parallelism, ...)`).

#### `parse_symbol_kind` fallback (`src/pipeline.rs:505`)

Unknown kind strings silently fall back to `SymbolKind::Function`. This could misclassify Dart classes or Python modules as functions.

**Recommendation:** Log a warning for unknown kinds. Consider adding a `SymbolKind::Unknown` variant.

#### `_visited` set in `resolve_refs` (`src/resolver.rs:29`)

The top-level `_visited` set is allocated but never read. The comment says "we don't actually chase chains" — this is dead code that should be removed.

#### `upsert_file` last_insert_rowid (`src/db.rs:164-174`)

The `last_insert_rowid()` returns 0 for `ON CONFLICT DO UPDATE`, and the code does a second query to fetch the real ID. This works but is a latent correctness issue: if another thread inserts between the upsert and the select (unlikely with the mutex, but the comment says "single-writer model" rather than guaranteeing it).

**Recommendation:** Use `RETURNING id` or `SELECT id FROM files WHERE path = ?1` unconditionally after the upsert.

#### `sutra_map` ignores workspace (`src/mcp.rs:241`)

`_ws` is assigned but unused — the map tool doesn't verify the workspace exists or use its root path for filtering.

#### Stale-check race in `cmd_serve_http` (`src/main.rs:179-182`)

The "already running" check is `TcpStream::connect(&addr)`. Between this check and the actual `TcpListener::bind`, another process could start. This is a TOCTOU race. In practice it's fine for a local dev tool, but the PID file written afterward (`src/main.rs:184-186`) doesn't clean up on crash, leading to stale PID files.

### 3.5 Medium — Performance

#### `git_cochange_files` is O(commits) with subprocess calls (`src/git.rs:64-83`)

For each commit hash, it shells out to `git show`. A workspace with 1,000 commits touching a file would spawn 1,000 processes.

**Recommendation:** Use `git log --numstat --format="" -- since=... -- path` to get all file changes in a single invocation, then aggregate in Rust.

#### `compute_rollups` loads all symbols and refs into memory (`src/pipeline.rs:391-449`)

For large workspaces (10K+ files), this loads the full symbol and ref tables into Rust `HashMap`s. This works but scales poorly.

**Recommendation:** Compute rollups in SQL:
```sql
UPDATE files SET fan_in_files = (
  SELECT COUNT(DISTINCT r.file_id) FROM refs r
  JOIN symbols s ON r.target_symbol_id = s.id
  WHERE s.file_id = files.id AND r.file_id != files.id
);
```

#### `serde_json::to_string_pretty` for all responses (`src/mcp.rs:213`)

Pretty-printed JSON is ~30-40% larger than compact. For MCP tool responses that may include thousands of symbols, this adds significant overhead.

**Recommendation:** Use `serde_json::to_string` (compact) for large responses, or make it configurable.

### 3.6 Low — Style & Nitpicks

- `ErrorData.tool` is `&'static str` but `ErrorData.argument` is `Option<String>` — inconsistent owned/borrowed mixing. Consider `Cow<'static, str>` for argument.
- `SymbolKind` and `RefContextKind` both have `as_str()` but no `FromStr` implementation. The pipeline has `parse_symbol_kind` and `parse_ref_context_kind` as free functions. These should be `impl FromStr` on the enums.
- The `workspace_id` field on `Db` is stored but only used by `workspace_id()` which appears to have no callers outside tests.
- `SKIP_DIRS` in `pipeline.rs:31` doesn't include `.git`, `dist`, `out`, `vendor`, or `__pycache__`. The `.claude` directory is excluded only because it starts with `.`, but the worktree directories under `.claude/worktrees/` are indexed (they appear in the PageRank map), inflating symbol counts.
- The `#[allow(deprecated)]` annotation on `any_service` (`src/main.rs:213`) should be tracked — either the API stabilizes or the code needs updating.

### 3.7 Dead Code

| Item | Location | Notes |
|------|----------|-------|
| `SmritiClient` | `src/smriti_client.rs` | Entire module is a stub, no callers |
| `_visited` set | `src/resolver.rs:29` | Allocated but never read |
| `workspace_id()` | `src/db.rs:134-136` | No external callers found |
| `SnapshotRow` | `src/db.rs:72-80` | Defined but never constructed in tool code |

---

## 4. Test Coverage

### Current State

- **Test files:** 3 (`tests/workspace_test.rs`, `tests/parse_rust_test.rs`, `tests/resolver_test.rs`)
- **Inline tests:** 1 (`src/parser/rust.rs:544-557`, smoke test only)
- **Untested source files:** 101/163 (62%)

### Critical Gaps

| Module | Risk | Missing Tests |
|--------|------|---------------|
| `pipeline.rs` | High | No test for the full parse pipeline, rollup computation, or stale detection |
| `db.rs` | High | No test for upsert, cascading delete, FTS5 sync, or IN-clause query |
| `resolver.rs` | Medium | Only 5 unit tests; no tests for import chains, cycle detection, or Dart imports |
| `tools/*` | Medium | Zero tests for any tool handler |
| `mcp.rs` | Medium | No integration test for the MCP server |
| `git.rs` | Low | Shell-out to `git` — hard to unit test, but no mock |

### Recommendations

1. **Priority 1:** Add DB-layer tests using `:memory:` SQLite. Test upsert idempotency, FTS5 sync, cascade deletes, and the IN-clause builder.
2. **Priority 2:** Add tool-handler unit tests. Each `handle()` is a pure function — easy to test with a temporary DB.
3. **Priority 3:** Add a pipeline integration test that parses a small fixture directory and verifies symbol/ref counts.

---

## 5. Security

All 840 findings are from `target/debug/build/libsqlite3-sys-*/out/bindgen.rs` — auto-generated FFI bindings with `unsafe extern "C"` function pointers. These are expected and come from the `libsqlite3-sys` crate, not from sutra's own code.

**No security issues in sutra's application code.** The codebase:
- Uses parameterized SQL queries throughout (no SQL injection).
- Does not log secrets or credentials.
- Shells out to `git` with fixed argument structure (no shell injection — args are passed as separate `Command::arg()` calls).
- The `dotenvy` load in `main.rs:60` is safe (`.ok()` discards missing-file errors).

---

## 6. Refactoring Priorities

| Priority | Item | Effort | Impact |
|----------|------|--------|--------|
| P0 | Decompose `parse_workspace` (CC 22→5) | Medium | Maintainability |
| P0 | Decompose `collect_symbols` (CC 28→5) | Medium | Maintainability |
| P1 | Add batch DB queries to fix N+1 patterns | Medium | Performance |
| P1 | Extract symbol-lookup helper from 4 tool files | Low | DRY |
| P1 | Extract `find_enclosing_symbol` to shared location | Low | DRY |
| P1 | Remove `SmritiClient` stub | Trivial | Dead code |
| P2 | Add `FromStr` for `SymbolKind` / `RefContextKind` | Low | Idiomatic Rust |
| P2 | Compute rollups in SQL instead of Rust | Medium | Performance |
| P2 | Optimize `git_cochange_files` (single `git log`) | Low | Performance |
| P3 | Add DB-layer tests | Medium | Correctness |
| P3 | Add tool-handler tests | Medium | Correctness |
| P3 | Filter `.claude/worktrees/` from indexing | Trivial | Accuracy |

---

## 7. Dependency Notes

From `Cargo.toml` (not fully read, but observed in code):
- `rmcp` — MCP framework (with `tower` feature for HTTP)
- `rusqlite` — SQLite (bundled, WAL mode)
- `tree-sitter` + `tree-sitter-rust` + `tree-sitter-dart` — parsers
- `blake3` — content hashing
- `clap` — CLI
- `serde` / `serde_json` / `schemars` / `toml` — serialization
- `parking_lot` — `Mutex` (not `std::sync::Mutex` — correct for DB single-writer)
- `chrono` — timestamps
- `tracing` + `tracing-subscriber` — logging
- `axum` — HTTP server
- `tokio` — async runtime
- `dotenvy` — `.env` loading

All are reasonable choices for this type of project.

---

## 8. Conclusion

Sutra is a well-structured v0.1 with a clean architecture and thoughtful error handling. The main risks are the two god functions (which make changes and testing difficult), the N+1 query patterns (which will bite at scale), and the thin test coverage. The refactoring priorities above are ordered by risk-reduction: decomposing the god functions and adding batch queries would significantly improve both maintainability and performance. Adding tests for the DB layer and tool handlers would catch regressions early.
