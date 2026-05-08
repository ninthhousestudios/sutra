# Code Review: v0.1.5 Feature Commits

**Reviewer**: GLM-5.1  
**Date**: 2026-04-28  
**Commits reviewed** (oldest → newest):

| SHA | Subject |
|---|---|
| `9ce1e5f` | feat: InsertSymbolParams struct + sutra_add_root MCP tool |
| `98e962b` | feat: PageRank population for files and symbols |
| `50bb21c` | feat: sutra-guard hooks (routing + modification guard) |
| `6a6bbb3` | perf: batch queries + incremental rollup recompute |
| `97070ce` | docs: update handoff for v0.1.5, add .gitignore, archive old handoff |

**Scope**: 18 files changed, +1059 / -201 lines.

---

## Summary

This series implements all five items from the previous handoff's "v0.1.5 candidates" list: InsertSymbolParams struct, `sutra_add_root` MCP tool, PageRank computation, guard hooks (routing + modification), and incremental rollup recompute. The last commit is documentation only.

Overall the code is well-structured and internally consistent. The issues below are ordered by severity.

---

## Critical Issues

### 1. PageRank N+1 write pattern — `update_file_pagerank` / `update_symbol_pagerank` called per-row

**Files**: `src/db.rs:243-258`, `src/pipeline.rs:545-572`  
**Commits**: `98e962b`, `6a6bbb3`

Both `update_file_pagerank` and `update_symbol_pagerank` issue individual `UPDATE` statements inside a loop:

```rust
for (i, &file_id) in file_ids.iter().enumerate() {
    db.update_file_pagerank(file_id, rank[i])?;  // 1 UPDATE per file
}
```

```rust
for &(sym_id, sym_refs) in syms {
    db.update_symbol_pagerank(sym_id, sym_pr)?;  // 1 UPDATE per symbol
}
```

The batch reads were correctly extracted in commit `6a6bbb3` (`all_symbol_file_map`, `all_resolved_refs`), but the writes were left as individual round-trips. For a workspace with 500 files and 5000 symbols, this is 5500 separate `UPDATE` statements, each acquiring/releasing the connection mutex.

**Recommendation**: Add `Db::batch_update_file_pagerank(files: &[(i64, f64)])` and `Db::batch_update_symbol_pagerank(symbols: &[(i64, f64)])` that use a single transaction + prepared statement with `execute_iterator` or a loop over `stmt.execute(params![pr, id])` inside one `conn.lock()` hold. This would reduce 5500 lock acquisitions to 2.

---

### 2. `sutra_add_root` race condition on workspace list

**File**: `src/mcp.rs:458-470`  
**Commit**: `9ce1e5f`

```rust
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

There is a TOCTOU gap between the read lock (checking `already_exists`) and the write lock (pushing the entry). Two concurrent `sutra_add_root` calls for the same workspace could both read `already_exists = false`, then both write — resulting in a duplicate entry.

**Recommendation**: Acquire a write lock from the start and do the check + insert atomically:

```rust
let already_exists = {
    let mut config = self.workspaces.write();
    let exists = config.workspace.iter().any(|w| w.id == ws_id);
    if !exists {
        workspace::add_workspace(&self.config.workspaces_path, entry.clone())?;
        config.workspace.push(entry.clone());
    }
    exists
};
```

The write lock is only held briefly and this is an uncommon path, so the contention cost is negligible.

---

### 3. `compute_pagerank` is still full-recompute, not incremental

**File**: `src/pipeline.rs:483-576`  
**Commit**: `98e962b`, `6a6bbb3`

Commit `6a6bbb3` added incremental dirty-set logic to `compute_rollups`, but `compute_pagerank` still recomputes from scratch every time. The pipeline calls both unconditionally after parsing:

```rust
compute_rollups(db, Some(&file_ids_needing_resolution))?;
compute_pagerank(db)?;  // always full
```

For incremental reparses (which only change a few files), this is wasteful. PageRank is a global property, but for small deltas the existing ranks are approximately correct — a few iterations of power iteration from the current vector would converge faster than starting from uniform.

**Recommendation**: Either (a) make `compute_pagerank` accept the existing ranks as a warm start and iterate from there (low effort, good enough), or (b) for now, document the trade-off and add a `TODO` so it's not forgotten.

---

## Important Issues

### 4. Guard fail-open silently swallows all errors

**File**: `src/bin/guard.rs:9-13`  
**Commit**: `50bb21c`

```rust
fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::SUCCESS, // fail-open
    }
}
```

Fail-open is a reasonable design choice for a hook, but silently discarding the error makes debugging very hard. If the guard binary can't parse stdin, can't find the DB, or hits a rusqlite error, there is zero indication anything went wrong. The user just sees Glob/Grep edits going through unguarded with no explanation.

**Recommendation**: Log errors to stderr (not stdout — that's the protocol channel) before returning SUCCESS:

```rust
Err(e) => {
    eprintln!("sutra-guard: {e}");
    ExitCode::SUCCESS
}
```

This preserves fail-open semantics while giving the operator a chance to notice problems.

---

### 5. `relativize_file_path` canonicalization can fail silently for non-existent files

**File**: `src/guard.rs:241-248`  
**Commit**: `50bb21c`

```rust
pub fn relativize_file_path(project_root: &Path, file_path: &Path) -> Option<String> {
    let canonical_root = project_root.canonicalize().ok()?;
    let canonical_file = file_path.canonicalize().ok().unwrap_or_else(|| file_path.to_path_buf());
    ...
}
```

When `file_path` doesn't exist on disk yet (e.g., the user is creating a new file with `Write`), `canonicalize` fails. The fallback `file_path.to_path_buf()` preserves the raw path, but if `canonical_root` has resolved symlinks while the raw `file_path` has not, `strip_prefix` will fail and the function returns `None`. The guard then returns `Ok(())` — allowing the edit unconditionally. This is a bypass scenario: a new file that would be in a load-bearing directory gets no protection.

**Recommendation**: For non-existent paths, try a logical strip_prefix instead of a canonical one:

```rust
let canonical_file = file_path.canonicalize().ok().unwrap_or_else(|| file_path.to_path_buf());
let result = canonical_file.strip_prefix(&canonical_root)
    .ok()
    .or_else(|| file_path.strip_prefix(project_root).ok())  // logical fallback
    .map(|p| p.to_string_lossy().into_owned());
```

---

### 6. Guard routing denies Glob/Grep unconditionally when index exists — no escape hatch for non-indexed queries

**File**: `src/bin/guard.rs:29-54`  
**Commit**: `50bb21c`

The routing guard denies Glob and Grep whenever the sutra index DB file exists, even if the user is searching for non-code content (e.g., searching `.toml` files, searching for string literals in markdown, searching for TODO comments). The current deny message only suggests `sutra_grep` / `sutra_find`, which search the indexed symbol set only. There's no way to bypass the routing guard per-query without fully disabling the guard via `SUTRA_GUARD_DISABLE=1`.

**Recommendation**: Consider adding a less-nuclear escape mechanism. Options:
- (a) Allow the user to pass a query parameter or env var like `SUTRA_GUARD_ALLOW_TEXT_SEARCH=1` that disables routing guard only (modification guard stays active).
- (b) Only deny when the Glob/Grep pattern looks like a symbol search (heuristic: no file extension in pattern, no `*.` prefix). This is fragile but pragmatic.

---

### 7. `compute_pagerank` symbol distribution collection is unnecessarily complex

**File**: `src/pipeline.rs:555-560`  
**Commit**: `6a6bbb3`

```rust
let mut file_symbols: HashMap<i64, Vec<(i64, usize)>> = HashMap::new();
for &(sym_id, file_id) in &sym_to_file.iter().map(|(&k, &v)| (k, v)).collect::<Vec<_>>() {
    let rc = ref_counts.get(&sym_id).copied().unwrap_or(0);
    file_symbols.entry(file_id).or_default().push((sym_id, rc));
}
```

This creates an intermediate `Vec<(i64, i64)>` via `.collect()` just to iterate over the HashMap entries. The `.collect()` is unnecessary — you can iterate the HashMap directly:

```rust
for (&sym_id, &file_id) in &sym_to_file {
    let rc = ref_counts.get(&sym_id).copied().unwrap_or(0);
    file_symbols.entry(file_id).or_default().push((sym_id, rc));
}
```

Minor, but the current code suggests the author was unsure about borrow-checker rules and added `.collect()` as a workaround.

---

### 8. `workspace_id_from_path` collision risk — two directories with same basename get same workspace ID

**Files**: `src/guard.rs:170-176`, `src/mcp.rs:453`  
**Commit**: `9ce1e5f`, `50bb21c`

```rust
pub fn workspace_id_from_path(root: &Path) -> String {
    root.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_lowercase()
        .replace(' ', "-")
}
```

Two workspaces at `/home/user/projects/foo` and `/home/user/other/foo` both get ID `"foo"`. The `add_root` code in `mcp.rs` uses the same logic and appends to the workspace list — if both are registered, the second `add_root` sees `already_exists = true` (based on ID match) and skips registration, then uses the wrong workspace's DB.

**Recommendation**: Include a disambiguation suffix. For example, hash the full root path and append the first 4 hex chars:

```rust
let base = root.file_name().and_then(|n| n.to_str()).unwrap_or("workspace");
let suffix = format!("{:04x}", fnv1a_64(root.to_string_lossy().as_bytes()) & 0xffff);
format!("{}-{}", base.to_lowercase().replace(' ', "-"), suffix)
```

Or, more simply, check that the root path matches when determining `already_exists`, not just the ID.

---

## Minor Issues

### 9. `dirs` module reimplementation in `main.rs`

**File**: `src/main.rs:413-418`  
**Commit**: `50bb21c`

```rust
mod dirs {
    use std::path::PathBuf;
    pub fn home_dir() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}
```

This only checks `$HOME`, which is wrong on macOS (`~/Library/Application Support` for some configs) and Windows. The `dirs` crate is already in scope via other dependencies (e.g., `rusqlite` pulls it in transitively). Consider using the real `dirs::home_dir()` instead, or at minimum document that this is intentionally minimal.

---

### 10. `InsertSymbolParams` uses lifetimes where owned strings might be cleaner

**File**: `src/db.rs:51-65`  
**Commit**: `9ce1e5f`

```rust
pub struct InsertSymbolParams<'a> {
    pub qualified_name: &'a str,
    pub short_name: &'a str,
    ...
}
```

The lifetime parameter propagates to every call site and makes the `sym()` helper in tests verbose. For a struct that's constructed inline and immediately consumed, owned `String` fields would be simpler. The current design is defensible (avoids allocation), but the allocation cost is negligible compared to the SQLite round-trip that follows.

**Severity**: Style/preference. Not a bug.

---

### 11. `.sutra/acks/` uses FNV-1a hashed filenames — no collision resistance guarantee

**File**: `src/guard.rs:196-199`  
**Commit**: `50bb21c`

```rust
pub fn ack_path(project_root: &Path, rel_path: &str) -> PathBuf {
    let digest = format!("{:016x}", fnv1a_64(rel_path.as_bytes()));
    project_root.join(".sutra").join("acks").join(digest)
}
```

FNV-1a is a non-cryptographic hash with known collision properties. Two different file paths that hash to the same 64-bit value would share an ack file, meaning acknowledging one would silently acknowledge the other. The probability is low for small workspaces but increases with scale.

**Recommendation**: Consider using the path itself (sanitized for filesystem safety) or a truncated SHA-256. Alternatively, embed the original path in the ack file content and verify on read.

---

### 12. `touch_ack` silently ignores `create_dir_all` failure

**File**: `src/guard.rs:203-205`  
**Commit**: `50bb21c`

```rust
if let Some(parent) = path.parent()
    && std::fs::create_dir_all(parent).is_err()
{
    return;
}
```

If `create_dir_all` fails (e.g., permissions), the function silently returns without writing the ack file. This means the guard will continue denying edits even after `sutra_impact` is called, because the ack was never written. The user gets no error feedback.

**Recommendation**: At minimum, log a warning via `eprintln!` or `tracing::warn!`. Better: propagate the error and let the MCP tool surface it.

---

### 13. Map importance formula mixes units

**File**: `src/tools/map.rs:18-19`  
**Commit**: `98e962b`

```rust
let pr_boost = (f.pagerank.unwrap_or(0.0) * 1000.0) as i64;
let importance = symbol_count + f.fan_in_files * 2 + f.blast_radius + pr_boost;
```

PageRank values are typically in [0, 1] (sum to 1 across all files). Multiplying by 1000 means a file with PR=0.2 gets a boost of 200, which dominates `symbol_count` and `fan_in_files` in most cases. The magic constant 1000 has no documented rationale and the truncation via `as i64` silently drops NaN/Inf (if they ever appear).

**Recommendation**: Document the scaling rationale. Consider normalizing PageRank to a 0-10 score (like qartez's health metric) before adding to importance, so the formula is interpretable.

---

### 14. `render_stdout` protocol detection is fragile

**File**: `src/guard.rs:148-168`  
**Commit**: `50bb21c`

```rust
if event_name == Some("BeforeTool") {
    Some(serde_json::json!({ "decision": "deny", "reason": reason }).to_string())
} else {
    Some(serde_json::json!({
        "hookSpecificOutput": { ... }
    }).to_string())
}
```

The two JSON shapes are for two different hook protocol versions. The detection is based on `event_name == "BeforeTool"` vs the default. If the protocol evolves and the event name changes, the guard would emit the wrong shape silently.

**Recommendation**: Extract the protocol version into a named constant or enum and match explicitly. At minimum, add a comment explaining which protocol version each branch handles.

---

### 15. `add_root` spawns parse without checking if a parse is already in progress

**File**: `src/mcp.rs:473-483`  
**Commit**: `9ce1e5f`

```rust
tokio::spawn(async move {
    match crate::pipeline::parse_workspace(&entry, &db, &config).await {
```

If `sutra_add_root` is called twice for the same workspace (the `already_exists` path), both calls spawn a parse. Two concurrent parses against the same DB would conflict on writes and could corrupt the index.

**Recommendation**: Add a per-workspace `AtomicBool` or `Mutex<bool>` "parsing_in_progress" flag. If already set, return a "parse already in progress" status instead of spawning another.

---

## Observations (Non-issues)

### Positive: Batch query refactoring

Commit `6a6bbb3` properly eliminates the N+1 read pattern in both `compute_rollups` and `compute_pagerank` by extracting `all_symbol_file_map()` and `all_resolved_refs()`. The shared `build_file_adjacency` function is a clean factorization. This is good work.

### Positive: Guard module separation

The guard logic is cleanly separated into a library module (`guard.rs`) and a thin binary wrapper (`bin/guard.rs`). The library functions (`evaluate`, `ack_is_fresh`, `render_stdout`) are all pure and testable independently. The binary does the I/O. This is a good separation of concerns.

### Positive: InsertSymbolParams refactor

The struct eliminates the 13-parameter function signature, which was a clippy waiting-to-happen. The test helper `sym()` in `calls-test.rs` and `impact_test.rs` is a nice pattern. The `seed_symbol` helper in `db-test.rs` is slightly less ergonomic (still lists all fields explicitly) but acceptable.

### Note: Docs commit is clean

`97070ce` is a straightforward docs-only change. The `.gitignore` additions are appropriate. The handoff update accurately reflects the v0.1.5 changes.

---

## Summary Table

| # | Severity | Category | File(s) | Description |
|---|---|---|---|---|
| 1 | **Critical** | Perf | `db.rs`, `pipeline.rs` | PageRank writes are N+1 — need batch UPDATE |
| 2 | **Critical** | Correctness | `mcp.rs` | TOCTOU race in `add_root` workspace registration |
| 3 | **Critical** | Perf | `pipeline.rs` | `compute_pagerank` always full-recompute, no warm start |
| 4 | **Important** | Observability | `bin/guard.rs` | Fail-open swallows errors silently — log to stderr |
| 5 | **Important** | Correctness | `guard.rs` | `relativize_file_path` fails for new-file Write ops |
| 6 | **Important** | UX | `bin/guard.rs` | Routing guard too aggressive — no per-query escape hatch |
| 7 | **Important** | Code quality | `pipeline.rs` | Unnecessary `.collect()` in symbol distribution loop |
| 8 | **Important** | Correctness | `guard.rs`, `mcp.rs` | Workspace ID collision for same-basename directories |
| 9 | Minor | Portability | `main.rs` | Reimplemented `dirs::home_dir()` — only checks `$HOME` |
| 10 | Minor | Style | `db.rs` | `InsertSymbolParams` lifetimes add verbosity; owned would be simpler |
| 11 | Minor | Correctness | `guard.rs` | FNV-1a ack path collisions (low probability) |
| 12 | Minor | Robustness | `guard.rs` | `touch_ack` silently ignores `create_dir_all` failure |
| 13 | Minor | Design | `map.rs` | PR×1000 scaling dominates importance formula, no rationale |
| 14 | Minor | Robustness | `guard.rs` | Protocol version detection is fragile string match |
| 15 | Minor | Correctness | `mcp.rs` | Duplicate `add_root` calls spawn concurrent parses |

---

## Recommended Priority

1. Fix #2 (TOCTOU race) — one-line change, prevents data corruption.
2. Fix #4 (stderr logging) — trivial, greatly improves debuggability.
3. Fix #15 (parse dedup) — prevents DB corruption from concurrent writes.
4. Fix #1 (batch pagerank writes) — significant perf improvement at scale.
5. Fix #5 (relativize fallback) — prevents guard bypass for new files.
6. Fix #8 (workspace ID collision) — prevents silent cross-workspace confusion.
7. Address #3 (warm-start pagerank) — perf improvement for incremental parses.
8. Remaining items are lower priority.
