# Sutra Code Review — Qwen3.6 Initial Review

**Date:** 2026-04-27
**Reviewer:** Qwen3.6
**Scope:** Full codebase review of sutra v0.1.0 (code intelligence MCP server)
**Stats:** 193 files, 171 source / 22 test, ~38.6K LOC (Rust), 5983 symbols, 215 import edges

---

## Executive Summary

Sutra is a Rust-based code intelligence MCP server that parses workspaces (Rust, Dart) using tree-sitter, stores symbol/ref/import data in SQLite, and exposes 14 MCP tools for code navigation and analysis. The codebase is well-structured for a v0.1 project with clear module boundaries and a consistent error-handling pattern. However, there are significant gaps in test coverage, performance concerns in the parsing pipeline, and several architectural improvements needed before production use.

**Overall Health Score: 4.7/10** — functional but needs hardening.

---

## 1. Architecture & Design

### Strengths

- **Clean module separation:** `lib.rs` exposes 11 modules with clear responsibilities: `config`, `daemon`, `db`, `error`, `git`, `mcp`, `parser`, `pipeline`, `resolver`, `smriti_client`, `tools`, `workspace`.
- **Consistent error handling:** `SutraError` enum with `thiserror` + structured `ErrorData` for MCP-compatible error responses is well-designed.
- **SQLite with WAL mode:** Correct PRAGMAs (WAL, foreign_keys, busy_timeout) and manual FTS5 sync is appropriate for a single-writer daemon.
- **DB caching pattern:** `Arc<Mutex<HashMap<String, Arc<Db>>>>` in `tools/mod.rs` avoids reopening databases per-request.
- **Dual transport:** HTTP + stdio MCP support is a good design choice.

### Concerns

#### 1.1 `src/db.rs` — No abstraction over raw SQL (Medium)

Every query is hand-written inline SQL. There's no query builder, no repository pattern, and no compile-time SQL validation (e.g., `sqlx`). This means:
- Typos in column names only surface at runtime.
- Schema changes require auditing every query manually.
- The `find_symbols_by_name` function (L357-441) has two nearly identical SQL queries differing only by a `WHERE` clause — a macro or builder would eliminate this duplication.

**Recommendation:** Consider `sqlx` with `compile_time` checks, or at minimum extract SQL strings into named constants and use a macro for the repeated `SELECT ... FROM symbols` projection.

#### 1.2 `src/mcp.rs` — Repetitive tool handler boilerplate (Medium)

Each of the 14 tool methods follows the same pattern:
```rust
let _ws = self.resolve_workspace(&args.workspace)?;
let db = self.get_db(&args.workspace)?;
let result = tools::X::handle(...).map_err(sutra_to_rmcp)?;
self.wrap_response(&db, result)
```
This is repeated verbatim across `sutra_map`, `sutra_outline`, `sutra_find`, `sutra_grep`, `sutra_read`, `sutra_impact`, `sutra_deps`. The analysis-tier tools add an `analysis_enabled` guard but otherwise follow the same shape.

**Recommendation:** Extract a helper like `fn run_tool<F>(&self, workspace: &str, f: F) -> Result<String, ErrorData>` to reduce boilerplate and make adding new tools a one-liner.

#### 1.3 `src/smriti_client.rs` — Stub module (Low)

This module is a 18-line no-op stub (`SmritiClient` with an empty `subscribe`). It's exported in `lib.rs` but does nothing. Either implement it or remove it to avoid confusion.

---

## 2. Performance

### 2.1 `src/pipeline.rs` — Sequential parsing (High)

The comment on L44-45 says: *"The `Db` is NOT Send/Sync, so we process files sequentially (v0.1)."* This is the single biggest performance bottleneck. For a workspace with 10K+ files, parsing sequentially will be prohibitively slow.

Additionally:
- `load_all_symbols` (L368-378) loads **every symbol from every file** into memory before resolution. For large workspaces this is O(N) memory and O(N*M) for resolution.
- `compute_rollups` (L391-449) loads all files, then for each file loads all its symbols, then walks all refs — this is O(F * S) DB queries where F = files and S = avg symbols per file.
- The BFS blast radius computation (L453-486) runs once per file, each time doing hash lookups. This is acceptable for small workspaces but will degrade.

**Recommendation:**
1. Make `Db` `Send` by replacing `parking_lot::Mutex<Connection>` with `rusqlite::Connection` + a connection pool (e.g., `r2d2` or `bb8`), or use `sqlite3_threadsafe()` + separate connections per thread.
2. Batch symbol insertion using transactions instead of individual `INSERT` calls.
3. Replace `load_all_symbols` with a single SQL query that joins symbols across files.

### 2.2 `src/tools/map.rs` — N+1 query pattern (Medium)

For each file in `all_files()`, the handler calls `db.find_symbols_by_file(f.id)` (L17). This is a classic N+1 query. With 10K files, that's 10K separate SELECTs.

**Recommendation:** Add a bulk query like `symbol_counts_by_file()` that returns `(file_id, count)` pairs in a single query.

### 2.3 `src/tools/health.rs` — Same N+1 issue (Medium)

Line 25: `.map(|f| db.find_symbols_by_file(f.id)...)` repeats the same pattern.

### 2.4 `src/db.rs` — Mutex serializes all reads (Medium)

`parking_lot::Mutex<Connection>` means even read-only queries are serialized. SQLite supports concurrent reads with WAL mode, but the mutex prevents this. For an MCP server handling concurrent tool calls, this will become a bottleneck.

**Recommendation:** Use `rusqlite`'s `Connection::open_with_flags` with `SQLITE_OPEN_READ_ONLY` for read paths, or use a read-pool + single-writer architecture.

---

## 3. Code Quality

### 3.1 `src/parser/rust.rs` — God function (High)

The `collect_symbols` function (L50-149) has CC=28 and handles 10+ node types in a single match block. It's the highest-complexity function in the codebase.

**Recommendation:** Extract each node-type handler into its own function (e.g., `extract_function`, `extract_struct`, `extract_impl`). This will also make it easier to add new language parsers.

### 3.2 `src/db.rs` — Long parameter lists (Medium)

- `insert_symbol` (L266-316): 13 parameters. Already has `#[allow(clippy::too_many_arguments)]` which suppresses the warning but doesn't fix the problem.
- `insert_snapshot` (L593-610): 6 parameters.

**Recommendation:** Introduce builder structs or parameter objects:
```rust
pub struct InsertSymbolParams<'a> {
    pub file_id: i64,
    pub qualified_name: &'a str,
    // ...
}
```

### 3.3 `src/db.rs` — Duplicated row-mapper logic (Low)

`map_file_row`, `map_symbol_row`, `map_ref_row`, `map_import_row` all follow the same `row.get(N)?` pattern. The column indices are positional and fragile — if the SELECT column order changes, the mapper breaks silently.

**Recommendation:** Use `rusqlite::types::FromRow` derive or named column access with a helper macro.

### 3.4 `src/pipeline.rs` — `delete_refs_for_file` is a no-op wrapper (Low)

Lines 386-388:
```rust
fn delete_refs_for_file(db: &Db, file_id: i64) -> Result<()> {
    db.delete_refs_by_file(file_id)
}
```
This function adds no value. The comment above it (L381-385) explains a design rationale that doesn't match the implementation. Remove it and call `db.delete_refs_by_file` directly.

### 3.5 `src/workspace.rs` — Unsafe atomic write (Medium)

Line 51: `let tmp_path = path.with_extension("toml.tmp");` — this uses `.with_extension()` which replaces the file extension, not appends. If the path is `workspaces.toml`, the temp file becomes `workspaces.tmp` (correct). But if the path is `/foo/bar/workspaces` (no extension), it becomes `/foo/bar.tmp` (wrong directory).

**Recommendation:** Use `path.with_extension("toml.tmp")` is fine for the expected `.toml` case, but add a comment or assertion. Better: use `format!("{}.tmp", path.display())` or the `tempfile` crate.

---

## 4. Correctness & Robustness

### 4.1 `src/db.rs` — `upsert_file` rowid bug (Medium)

Lines 164-173 attempt to handle the `ON CONFLICT DO UPDATE` rowid reuse case:
```rust
let id = conn.last_insert_rowid();
if id == 0 {
    let real_id: i64 = conn.query_row("SELECT id FROM files WHERE path = ?1", params![path], |row| row.get(0))?;
    return Ok(real_id);
}
```
The assumption that `last_insert_rowid() == 0` means "was an update" is **not guaranteed** by SQLite. The rowid is only 0 if the table has a row with `id=0` (which can't happen with AUTOINCREMENT), but this is still a fragile heuristic.

**Recommendation:** Always do `SELECT id FROM files WHERE path = ?` after the upsert, or use `RETURNING id` (SQLite 3.35+).

### 4.2 `src/resolver.rs` — Unused `_visited` field (Low)

Line 29: `let mut _visited: HashSet<&str> = HashSet::new();` — this is created but never used. The real `visited` set is created per-call in `find_via_imports` (L81). Remove the dead code.

### 4.3 `src/resolver.rs` — Multiple global match fallback (Medium)

Lines 105-114: When there are multiple global matches, the resolver picks `global_matches[0]` with a comment "just pick the first one (stable ordering from the DB)." This is non-deterministic — SQLite doesn't guarantee ordering without `ORDER BY`.

**Recommendation:** Add `ORDER BY qualified_name` or prefer matches in imported files.

### 4.4 `src/git.rs` — N+1 git subprocess calls (Medium)

`git_cochange_files` (L35-89) runs `git show --name-only` once **per commit**. For a file with 500 commits in the window, that's 500 subprocess invocations.

**Recommendation:** Use a single `git log --format=%H --name-only --since="N days ago" -- path` call and parse the output.

### 4.5 `src/main.rs` — PID file race condition (Low)

Lines 184-186:
```rust
let pid_path = config.db_dir.join("sutra.pid");
std::fs::create_dir_all(&config.db_dir)?;
std::fs::write(&pid_path, std::process::id().to_string())?;
```
The PID file is written after the port check (L179), creating a TOCTOU race. Also, the PID file is never cleaned up on panic/crash.

**Recommendation:** Use a proper lock file with `flock` or the `fs2` crate.

### 4.6 `src/daemon.rs` — Blocking reparse in async context (Medium)

Line 63: `pipeline::parse_workspace(ws, &db, &self.config).await` is called inside a `tokio::spawn` loop. While the function is `async`, the actual parsing work is CPU-bound and synchronous (tree-sitter parsing, file I/O). This will block the tokio runtime.

**Recommendation:** Wrap CPU-bound work in `tokio::task::spawn_blocking`.

---

## 5. Security

### 5.1 Path traversal risk (Medium)

`src/tools/read.rs` L37: `let abs_path = workspace_root.join(&file.path);` — if `file.path` contains `..` sequences, this could escape the workspace root. While the path comes from the parser (not user input), a malicious workspace could craft file paths.

**Recommendation:** Add `abs_path.starts_with(workspace_root)` validation before reading.

### 5.2 SQL injection via dynamic IN clause (Low)

`src/db.rs` L523-531: `find_files_referencing_symbols` builds a dynamic SQL string with `format!`. While the parameters are `i64` values (not user strings), this is still a pattern that invites future bugs.

**Recommendation:** Use a fixed-size IN clause or a temp table for large batches.

### 5.3 `target/` directory in index (Low)

The security scan found 840 findings, all in `target/debug/build/libsqlite3-sys-*/out/bindgen.rs` — auto-generated FFI bindings. These are false positives but indicate the security scanner is scanning build artifacts.

**Recommendation:** Add `target/` to the `.gitignore` equivalent for the security scanner, or configure the scanner to exclude build directories.

---

## 6. Test Coverage

### Critical Gap: 101/163 source files untested (62%)

Only 22 test files exist, and most are parser fixture tests. The following high-value modules have **zero tests**:

| Module | PageRank | Blast Radius | Priority |
|--------|----------|-------------|----------|
| `src/parser/mod.rs` | 0.0219 | 10 | Critical |
| `src/resolver.rs` | 0.0034 | 0 | High |
| `src/daemon.rs` | 0.0034 | 0 | High |
| `src/git.rs` | 0.0034 | 0 | High |
| `src/tools/*.rs` (all 12) | 0.0034 | 0 | High |
| `src/mcp.rs` | 0.0034 | 0 | Medium |
| `src/db.rs` (beyond basic) | 0.0710 | 60 | Critical |

The only test in `src/parser/rust.rs` is a 7-line smoke test (`smoke_parse_function`).

**Recommendation:** Prioritize tests for:
1. `src/db.rs` — CRUD operations, FTS5 sync, cascade deletes
2. `src/resolver.rs` — local resolution, import resolution, ambiguity handling
3. `src/tools/impact.rs` and `src/tools/calls.rs` — BFS logic, edge cases
4. `src/pipeline.rs` — incremental parse (hash-based skip), dirty marking

---

## 7. Language Support

### 7.1 Only Rust and Dart parsers exist (Medium)

The `parser/mod.rs` dispatch (L115-128) handles only `"rust"` and `"dart"`. Any other language returns an empty `ParseResult` with `parsed_ok: false`. The `pipeline.rs` `extensions_for_language` (L34-40) has the same limitation.

**Recommendation:** Add a clear error message for unsupported languages instead of silently returning empty results. Consider a plugin architecture for language parsers.

### 7.2 Dart parser not reviewed

The Dart parser (`src/parser/dart.rs`) exists but was not examined in detail. Given the Rust parser's complexity (557 lines), the Dart parser likely has similar issues.

---

## 8. Miscellaneous

### 8.1 Stale worktree directories (Low)

The index includes files from `.claude/worktrees/agent-*/` — these are Claude Code worktrees that should not be indexed. They inflate the file count and create duplicate symbol entries.

**Recommendation:** Add `.claude/` to `SKIP_DIRS` in `pipeline.rs` (L31).

### 8.2 `_config` unused parameter (Low)

`parse_workspace` (L45-49) takes `_config: &Config` but only uses it for... nothing. The parameter is prefixed with `_` indicating it's intentionally unused.

**Recommendation:** Either use it (for `parse_parallelism`) or remove it.

### 8.3 Hardcoded thresholds (Low)

`src/tools/impact.rs` L80-101: Risk thresholds (15 callers = high, 5 = medium, 20 files = high, 8 = medium) are hardcoded. These should be configurable.

### 8.4 `SutraServer::Clone` reinitializes `tool_router` (Low)

`src/mcp.rs` L144-154: `Clone` for `SutraServer` creates a fresh `tool_router` via `Self::tool_router()`. This is likely intentional (rmcp requirement) but worth documenting.

---

## 9. Priority Action Items

| Priority | Issue | File | Effort |
|----------|-------|------|--------|
| P0 | Add test coverage for db, resolver, tools | Multiple | High |
| P0 | Make parsing parallelizable | `pipeline.rs` | High |
| P1 | Fix N+1 queries in map.rs, health.rs | `tools/*.rs` | Low |
| P1 | Add path traversal guard | `tools/read.rs` | Low |
| P1 | Fix `upsert_file` rowid heuristic | `db.rs` | Low |
| P2 | Extract tool handler boilerplate | `mcp.rs` | Medium |
| P2 | Reduce `collect_symbols` complexity | `parser/rust.rs` | Medium |
| P2 | Replace `_visited` dead code | `resolver.rs` | Low |
| P2 | Exclude `.claude/` from indexing | `pipeline.rs` | Low |
| P3 | Introduce parameter objects for long param lists | `db.rs` | Medium |
| P3 | Use `spawn_blocking` for CPU work in daemon | `daemon.rs` | Low |
| P3 | Add unsupported language error | `parser/mod.rs` | Low |

---

## 10. Conclusion

Sutra v0.1.0 is a solid foundation for a code intelligence MCP server. The architecture is clean, the error handling is well-designed, and the SQLite + FTS5 approach is appropriate for the use case. The primary concerns are:

1. **Test coverage** — 62% of source files are untested, including the most critical modules.
2. **Performance** — Sequential parsing, N+1 queries, and mutex-serialized reads will not scale.
3. **Robustness** — The `upsert_file` rowid heuristic, resolver's non-deterministic fallback, and path traversal risk need addressing.

With these issues resolved, sutra would be production-ready for medium-sized workspaces.
