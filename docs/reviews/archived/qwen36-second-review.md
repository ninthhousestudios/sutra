# Code Review: sutra v0.1.5 — Five-Commit Batch

**Reviewed:** 2026-04-28
**Commits:** `9ce1e5f` → `98e962b` → `50bb21c` → `6a6bbb3` → `97070ce`
**Total:** +1059 / -201 lines across 18 files

---

## Executive Summary

This batch implements all five items from the prior handoff's v0.1.5 candidate list: `InsertSymbolParams` refactor, `sutra_add_root` MCP tool, PageRank computation, guard hooks (routing + modification), and incremental rollup recompute with batch queries. All 107 tests pass, zero clippy warnings. The scope is ambitious for a single session — roughly a week's worth of feature work compressed into ~50 minutes of wall-clock commits — but the quality is solid overall with a handful of correctness and design concerns.

**Verdict:** Ship with fixes for 2 correctness issues and 3 medium-priority improvements before calling it done.

---

## Commit-by-Commit Review

### 1. `9ce1e5f` — InsertSymbolParams struct + sutra_add_root MCP tool

**What changed:** Replaced 13-parameter `insert_symbol()` with `InsertSymbolParams<'a>` struct. Added `sutra_add_root` MCP tool. Changed `WorkspacesConfig` from `Arc<Mutex<>>` to `Arc<RwLock<>>` for concurrent reads.

**Good:**
- The params struct is a clear win. The old 13-arg signature was a clippy `too_many_arguments` violation waiting to happen.
- All 27 call sites updated across 4 test files — no stragglers.
- `RwLock` is the right choice: `Daemon::check_stale_workspaces` and MCP tool handlers only need read access; `add_root` is the sole writer.
- `sutra_add_root` validates that the path is absolute and exists before proceeding.

**Issues:**

#### C1: TOCTOU race in `sutra_add_root` (Medium)
```rust
// src/mcp.rs:473-482
let already_exists = {
    let config = self.workspaces.read();
    config.workspace.iter().any(|w| w.id == ws_id)
};

if !already_exists {
    workspace::add_workspace(&self.config.workspaces_path, entry.clone())
        .map_err(sutra_to_rmcp)?;
    self.workspaces.write().workspace.push(entry.clone());
}
```

Between the read-lock check and the write-lock push, a concurrent `add_root` call with the same workspace could slip through and write a duplicate entry to the TOML file. In practice this is unlikely (single-agent session), but the fix is trivial: hold the write lock for the entire check-and-insert:

```rust
let mut config = self.workspaces.write();
let already_exists = config.workspace.iter().any(|w| w.id == ws_id);
if !already_exists {
    workspace::add_workspace(&self.config.workspaces_path, entry.clone())?;
    config.workspace.push(entry.clone());
}
```

#### C2: `add_workspace` writes to disk but is not atomic (Low)
`workspace::add_workspace` reads the TOML, appends, and writes back. If two processes do this concurrently, one write can clobber the other. Not a practical concern for the current single-user model, but worth noting if sutra ever goes multi-tenant.

#### C3: `tokio::spawn` error is silently swallowed (Low)
The parse runs in a `tokio::spawn` with only `tracing::error!` on failure. The caller gets an immediate "registered, parsing" response with no way to learn if parsing later failed. Consider returning a task ID or at least logging the workspace ID in the error (it does — `ws_id_bg`). Acceptable as-is for v0.1.5.

---

### 2. `98e962b` — PageRank population for files and symbols

**What changed:** Power iteration PageRank on the file dependency graph. Symbol-level PR distributed from file PR by incoming ref weight. `sutra_map` ranking now includes `pr_boost`.

**Good:**
- Standard PageRank implementation with damping=0.85, epsilon=1e-6, max 100 iterations — textbook correct.
- Dangling node handling (distribute evenly) is present.
- Symbol-level distribution by incoming ref weight is a reasonable heuristic.
- `sutra_map` now exposes `pagerank` in its JSON output — good for transparency.

**Issues:**

#### C4: `compute_pagerank` always runs full recomputation (Medium)
Unlike `compute_rollups` which got incremental support in commit 4, `compute_pagerank` always iterates over all files and symbols. On a large workspace (10K+ files), this is O(V+E) per parse even when only 1 file changed. PageRank is inherently global (a change anywhere can shift all ranks), but an incremental approximation (e.g., only re-iterate from changed nodes) would be a worthwhile v0.2.0 optimization.

#### C5: Symbol PR distribution heuristic is lossy (Low)
The symbol-level PR distributes file PR proportionally to incoming ref counts. Symbols with zero refs get an equal split of the file's PR. This means a file with 100 symbols where only 1 is referenced will give that one symbol ~99% of the file's PR and the other 99 symbols ~0.01% each. This is defensible (unused symbols *are* less important) but worth documenting so users understand why `pagerank` on rarely-referenced symbols is near-zero.

#### C6: `pr_boost` scaling factor is magic (Low)
```rust
let pr_boost = (f.pagerank.unwrap_or(0.0) * 1000.0) as i64;
```
The `* 1000.0` multiplier makes PageRank comparable to `symbol_count` and `fan_in_files * 2` in the importance formula. This works for the current codebase but will behave unpredictably on repos with very different graph densities. A normalized weighting (e.g., `w1 * normalized(symbol_count) + w2 * normalized(fan_in) + w3 * pagerank`) would be more robust. Not urgent — just flag it as a known tuning knob.

---

### 3. `50bb21c` — sutra-guard hooks (routing + modification guard)

**What changed:** New `sutra-guard` binary for PreToolUse hooks. Routing guard denies Glob/Grep when sutra index exists. Modification guard blocks edits to load-bearing files until `sutra_impact` is called. Ack protocol via `.sutra/acks/` with FNV-1a hashed paths and 600s TTL. CLI install/uninstall manages Claude Code settings.json.

**Good:**
- Fail-open design is correct for a guard: errors never block the user's workflow.
- `SUTRA_GUARD_DISABLE=1` escape hatch is present.
- Environment variable configuration for thresholds (`SUTRA_GUARD_PAGERANK_MIN`, etc.) is well-designed.
- FNV-1a for ack file naming is a good choice: fast, no external crypto dependency, collision-resistant enough for this use case.
- The guard correctly opens SQLite read-only with `SQLITE_OPEN_NO_MUTEX` — it won't contend with the main sutra process.
- Install removes qartez hooks before adding sutra hooks — clean migration path.

**Issues:**

#### C7: Guard binary always returns `ExitCode::SUCCESS` — hook may not block (High — Correctness)
```rust
// src/bin/guard.rs:9-14
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::SUCCESS, // fail-open
    }
}
```

The guard communicates its decision via stdout JSON, NOT via the exit code. This means the hook framework sees success regardless of whether the decision is Allow or Deny. The blocking behavior depends entirely on Claude Code parsing the stdout JSON format correctly. If the JSON format doesn't match what Claude Code expects for `PreToolUse` hooks, the guard is a no-op.

The `render_stdout` function produces two formats:
- `BeforeTool` event: `{"decision": "deny", "reason": "..."}`
- `PreToolUse` event: `{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", ...}}`

**Verify that the `PreToolUse` format matches Claude Code's expected schema.** If it doesn't, the modification guard will never actually block edits.

#### C8: Routing guard matches on lowercase tool name but modification guard matches on exact case (Medium)
```rust
// Routing guard (lowercase):
if matches!(tool_lower.as_str(), "glob" | "grep" | "grep_search") {

// Modification guard (exact case):
if !matches!(hook.tool_name.as_str(), "Edit" | "Write" | "MultiEdit" | "replace" | "write_file") {
```

The routing guard lowercases `tool_name` before matching, but the modification guard matches on the original case. If Claude Code sends `"edit"` (lowercase) instead of `"Edit"`, the modification guard will pass through. Be consistent — lowercase both or document the expected case.

#### C9: `relativize_file_path` canonicalizes both paths but fallback is asymmetric (Low)
```rust
pub fn relativize_file_path(project_root: &Path, file_path: &Path) -> Option<String> {
    let canonical_root = project_root.canonicalize().ok()?;
    let canonical_file = file_path.canonicalize().ok().unwrap_or_else(|| file_path.to_path_buf());
    // ...
}
```

If the file doesn't exist yet (e.g., a `Write` tool creating a new file), `canonicalize()` fails and falls back to the raw path. But `canonical_root` is always canonicalized. This means `strip_prefix` will fail if the raw file path uses different path components (e.g., `./src/foo.rs` vs `/home/user/project/src/foo.rs`). The guard silently allows the edit in this case, which is correct (fail-open) but means the modification guard won't protect new files.

#### C10: `touch_ack` uses `fs::write` which is not atomic (Low)
If the process crashes mid-write, the ack file could be partially written. On the next check, `ack_is_fresh` would read the mtime (which is set when the file is created) but the content might be garbage. Since `ack_is_fresh` only checks mtime (not content), this is actually fine — the mtime is set by the filesystem on file creation, not by the write. No fix needed, just noting the reasoning.

#### C11: `find_project_root` walks up to filesystem root on non-git projects (Low)
```rust
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}
```

On a non-git project inside the user's home directory, this walks all the way to `/` before returning `None`. Add a maximum depth limit (e.g., 20 levels) to bound the search.

---

### 4. `6a6bbb3` — Batch queries + incremental rollup recompute

**What changed:** Added `all_symbol_file_map` and `all_resolved_refs` batch queries. Extracted `build_file_adjacency` for shared use. `compute_rollups` accepts optional dirty set for incremental recomputation. `compute_pagerank` also uses batch queries.

**Good:**
- The N+1 elimination is well-executed. `build_file_adjacency` replaces the per-file `find_symbols_by_file` + `find_refs_in_file` loop with two single-query batch operations.
- `compute_rollups` incremental mode correctly expands the dirty set to include depth-1 neighbors (both fan-in dependents and outgoing targets).
- `parse_workspace` passes `file_ids_needing_resolution` as the dirty set — correct integration.

**Issues:**

#### C12: Unnecessary allocation in `compute_pagerank` symbol grouping (Low — Perf)
```rust
// src/pipeline.rs:557
for &(sym_id, file_id) in &sym_to_file.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>() {
```

This allocates a `Vec` just to iterate over entries that are already available from `sym_to_file.iter()`. Replace with:
```rust
for (&sym_id, &file_id) in &sym_to_file {
    let rc = ref_counts.get(&sym_id).copied().unwrap_or(0);
    file_symbols.entry(file_id).or_default().push((sym_id, rc));
}
```

#### C13: `compute_rollups` rebuilds full adjacency graph even for incremental mode (Medium)
When `changed_file_ids` is `Some`, `compute_rollups` still calls `build_file_adjacency` which scans ALL symbols and ALL refs. For small incremental changes, the adjacency graph could be computed incrementally too. This is a v0.2.0 optimization — the current approach is correct, just not optimal.

---

### 5. `97070ce` — Docs update, .gitignore, archive handoff

**Good:**
- `.gitignore` correctly excludes `.sutra/` (ack files), `target/`, and `.agents/`.
- Handoff archive pattern (`.handoffs/YYYY-MM-DD.md`) is clean.
- Updated `docs/handoff.md` accurately reflects v0.1.5 state and sets up v0.2.0 candidates.

**No issues.**

---

## Cross-Cutting Concerns

### T1: `parse_workspace` ignores `config` parameter
```rust
// src/pipeline.rs:308
let _ = config;
```

The `parse_parallelism` field from `Config` is unused. The file parsing loop is sequential (`for file_path in &source_files`). This is a missed opportunity — `parse_single_file` calls are independent and could be parallelized with `rayon` or `tokio::spawn`. Not a bug, but the `let _ = config` is a code smell that should be addressed.

### T2: No guard for `sutra_add_root` against path traversal
The `sutra_add_root` tool accepts any absolute directory path. A malicious agent could register `/` or `/etc` as a workspace. Consider validating that the path is within a reasonable boundary (e.g., under the user's home directory or the current working directory).

### T3: `sutra_impact` ack touches the file path from the response, not the symbol's file
```rust
// src/mcp.rs:327-329
if let Some(file_path) = result["file"].as_str() {
    guard::touch_ack(&ws.root, file_path);
}
```

This is correct — the impact tool now returns the file path, and the ack is touched for that file. But note that if the user runs `sutra_impact` for a symbol and then edits a *different* file in the same workspace, the ack won't help. The ack is per-file, which is the right design, but users should understand the scope.

### T4: `compute_pagerank` runs after `compute_rollups` but uses the same graph
Both functions build the file dependency graph independently. `compute_rollups` builds it via `build_file_adjacency`, and `compute_pagerank` builds it inline from `all_symbol_file_map` + `all_resolved_refs` + `import_edges`. These could share the adjacency graph to avoid redundant computation. Minor perf improvement.

---

## Test Coverage Assessment

| Area | Tests | Notes |
|---|---|---|
| `InsertSymbolParams` | Covered | All existing tests updated, no new behavior to test |
| `sutra_add_root` | **Not tested** | No integration test for the MCP tool. Should test: register new workspace, re-register existing, async parse completion |
| PageRank | **Not tested** | No unit test for `compute_pagerank`. Should test: convergence on a known graph, dangling node handling, symbol distribution |
| Guard hooks | **Not tested** | No test for `guard::evaluate`, `ack_is_fresh`, `relativize_file_path`. Should test: allow/deny thresholds, ack TTL, path canonicalization |
| Incremental rollups | **Partially tested** | Existing `test_incremental_reparse` tests the pipeline but doesn't verify that only dirty files are recomputed |

**Recommendation:** Add at least 5-8 tests for PageRank and guard logic before v0.1.5 release.

---

## Summary of Issues

| ID | Severity | Commit | Description |
|---|---|---|---|
| C7 | **High** | 50bb21c | Guard exit code always SUCCESS — blocking depends on JSON format matching Claude Code's schema |
| C1 | Medium | 9ce1e5f | TOCTOU race in `sutra_add_root` check-then-insert |
| C4 | Medium | 98e962b | PageRank always full recomputation, no incremental mode |
| C8 | Medium | 50bb21c | Inconsistent tool name casing between routing and modification guards |
| C13 | Medium | 6a6bbb3 | Adjacency graph rebuilt fully even for incremental rollup |
| T1 | Medium | all | `config.parse_parallelism` unused — sequential parsing |
| C2 | Low | 9ce1e5f | `add_workspace` TOML write not atomic |
| C3 | Low | 9ce1e5f | Spawned parse error only logged, no callback to caller |
| C5 | Low | 98e962b | Symbol PR distribution heuristic is lossy (document, don't fix) |
| C6 | Low | 98e962b | `pr_boost` scaling factor is magic number |
| C9 | Low | 50bb21c | New files bypass modification guard due to canonicalize mismatch |
| C10 | Low | 50bb21c | `touch_ack` not atomic (safe because only mtime matters) |
| C11 | Low | 50bb21c | `find_project_root` can walk to filesystem root |
| C12 | Low | 6a6bbb3 | Unnecessary Vec allocation in pagerank symbol grouping |
| T2 | Low | 9ce1e5f | No path traversal guard on `sutra_add_root` |
| T4 | Low | 98e962b | Adjacency graph computed twice (rollups + pagerank) |

---

## Recommended Actions (Priority Order)

1. **Verify C7:** Confirm that the `PreToolUse` JSON format matches Claude Code's hook schema. If it doesn't, the entire modification guard is non-functional.

2. **Fix C1:** Hold write lock for the full check-and-insert in `sutra_add_root`.

3. **Fix C8:** Lowercase the tool name before the modification guard match, or document the expected case convention.

4. **Add tests:** PageRank convergence, guard evaluate thresholds, `sutra_add_root` MCP contract.

5. **Fix C12:** Remove unnecessary Vec allocation in pagerank symbol grouping (one-liner).

6. **Address T1:** Either use `config.parse_parallelism` for parallel file parsing or remove the field and document the decision.
