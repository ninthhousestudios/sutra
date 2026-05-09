# Bug: scheduler wedges on a single workspace, leaves all indexes stale

**Date:** 2026-05-08
**Severity:** High — silent degradation. Daemon stays alive, MCP queries keep returning, but every workspace's index ages indefinitely with no recovery.
**Reproduction:** observed in production. Last working scheduler tick at `19:15:20Z`; bug noticed at `~20:51Z` (1h 35m of silent staleness across 30+ workspaces).

## Symptom

`sutra_find(workspace="yojana", name="CreateTaskParams")` returned:

```json
{
  "as_of": "2026-05-08T19:05:34.911744448+00:00",
  "is_stale": true,
  "matches": [{ "_freshness": "fresh", ... }]
}
```

The `as_of` was 1h 45m before the query. Per-match freshness was `"fresh"` (file mtime had not changed since indexing), yet the top-level `is_stale` was `true`. The yojana code had not been touched in days.

The `is_stale` verdict was technically correct: the snapshot really was 1h 45m old, beyond the 600s threshold. The real question was *why the daemon hadn't reparsed*.

## Root cause chain

### 1. The scheduler is wedged on `josh`

`src/daemon.rs:235-296` — `check_stale_workspaces`:

```rust
for ws in &entries {
    ...
    match tokio::spawn(async move {
        pipeline::parse_workspace(&ws_clone, &db_clone, &config_clone).await
    })
    .await    // ← awaits inline
    { ... }
}
```

`tokio::spawn(...).await` waits for the spawned task to complete before the next iteration. The whole scheduler tick is therefore serial: one workspace at a time, blocking. The interval timer cannot fire its next tick until `check_stale_workspaces` returns.

The last journald log line is:

```
19:15:20Z INFO sutra::pipeline: walked workspace workspace=josh files_found=4132
```

No completion, no failure. The future inside the spawned task has been pending since 19:15:20. The scheduler has been waiting on it for over an hour. Daemon process state confirms it: PID 444834 alive, 14 threads, 1 running, 13 sleeping — not crashed, just blocked.

### 2. Parse failures don't record a snapshot

`src/pipeline.rs:325` and `:405` — `parse_workspace` and `parse_changed_files` only call `record_snapshot` on the success path. When the first two `josh` reparse attempts failed at `19:05:44` and `19:10:24` with `database is locked`, no snapshot was inserted.

`db.last_parse_time()` reads `MAX(timestamp) FROM snapshots`. With no row, it returns `Ok(None)`. The staleness check at `daemon.rs:262` treats `Ok(None)` as stale:

```rust
let is_stale = match db.last_parse_time() {
    Ok(Some(ts)) => ...,
    Ok(None) => true,    // ← always stale if never recorded
    Err(_) => true,
};
```

So `josh` is *permanently* stale until a successful parse lands. Every scheduler tick re-targets it.

### 3. The `josh` workspace is pathological

`~/.sutra/workspaces.toml`:

```toml
id = "josh"
root = "/home/josh"
```

Root `/home/josh` transitively contains every other workspace root (`sutra`, `chitta`, `fallow`, `manas`, `innerorbits`, etc.). The smriti FS-event watcher (`src/daemon.rs:73-125` `smriti_watcher_loop`) routes every event to every workspace whose root prefixes the event path, so any change anywhere under `/home/josh` lands in the `josh` debounce buffer in addition to the specific workspace's buffer.

Net effect: the smriti watcher's `flush_debounced` path frequently calls `pipeline::parse_changed_files(&josh, ...)` while the scheduler is also calling `pipeline::parse_workspace(&josh, ...)`. Both hold writes against `~/.sutra/josh/index.db`. SQLite is configured with `busy_timeout = 5000` (`src/db.rs:154`); on a 4132-file traversal that easily exceeds 5s, contention produces `database is locked`.

### 4. Why the third attempt hung instead of failing fast

Unconfirmed. Hypothesis: the busy-timeout retry loop inside rusqlite or a transaction held by a long-running PageRank/rollup pass (`src/pipeline.rs:447-457` `post_parse_sequence` → `graph::compute_pagerank_with_adjacency`) is now waiting on a lock that nothing will release. The smriti watcher loop is a separate task on the same runtime and is presumably also alive (one running thread, smriti polls are awake), so its writes may be holding the lock.

This sub-bug is the trigger but not the structural defect. Even if josh's parse always failed cleanly in 5s, the fact that *one* workspace can wedge the entire fleet's scheduling is the bug.

## Timeline (UTC)

| Time | Event |
|---|---|
| 18:35 | Full scheduler cycle, all workspaces reparsed cleanly. |
| 18:50 | Full cycle. |
| 19:05:19–19:05:35 | Full cycle through 29 workspaces. |
| 19:05:35 | `workspace josh is stale, triggering reparse` |
| 19:05:44 | `reparse failed for josh: database error: database is locked` |
| 19:10:19 | Tick: only josh re-evaluated as stale (others <600s old). Fails again 5s later. |
| 19:15:19 | Tick: only josh stale. Walks 4132 files at 19:15:20. **Never returns.** |
| 19:15:20 → present | Scheduler blocked. No further `check_stale_workspaces` invocations. All workspaces age past threshold with no refresh. |
| ~20:51 | Bug observed: `sutra_find` on yojana returns 105-minute-old `as_of`, `is_stale: true`. |

## Why the symptoms were misleading

- **Per-match `_freshness: "fresh"`**: file mtime ≤ snapshot mtime, so the matched symbol's source file *had* been seen by the indexer at snapshot time. That field doesn't say "the index is up to date" — it says "this file hasn't been edited since we last looked".
- **`is_stale: true`**: correctly flagged the snapshot age.
- **No error visible to the MCP client**: the daemon happily serves queries from a stale index. There is no surfaced "the scheduler died" signal.

## Fixes (priority order)

### 1. Decouple the scheduler from per-workspace parse latency

`src/daemon.rs:235-296`. Don't `await` each `tokio::spawn` inline.

Options:
- **Fire-and-forget per workspace**, with a per-workspace `Mutex<()>` (or `tokio::sync::Mutex`) keyed by `ws.id` to prevent two concurrent reparses of the same workspace. The mutex lives in `Daemon` alongside `last_watcher_refresh`. If the lock is held, skip this tick for that workspace.
- **JoinSet with bounded concurrency**, e.g. 4 reparses at once, so a slow workspace can't starve the rest while still bounding fleet-wide CPU/IO.

Either way, **one workspace's slowness must not block ticks of other workspaces.**

### 2. Record a snapshot even on failure

`src/pipeline.rs:325` and `:405`. Always insert a snapshot row at the end of `parse_workspace` / `parse_changed_files`, including when an inner step errored. The schema already has `parse_errors`. Add a `failed: bool` column (or repurpose `parse_errors >= 1 && files_parsed == 0`) so `is_stale` doesn't treat "we tried and failed at time T" the same as "we never tried".

Without this, any workspace that fails its first reparse becomes a permanent reparse target on every tick — feeding bug #1.

### 3. Bound `parse_workspace` with a timeout

Wrap the spawned future in `tokio::time::timeout(Duration::from_secs(60), ...)`. If a reparse exceeds the timeout, log + abort + record a failed snapshot. This prevents infinite hangs from wedging the per-workspace mutex.

### 4. Fix the `josh` workspace registration

Root `/home/josh` was almost certainly registered by accident — one of the agents must have called `sutra_add_root("/home/josh")`. This workspace duplicates indexing of every other workspace, contributes nothing the per-project workspaces don't, and is the largest single contributor to scheduler load (4132 files). Two ways to handle:

- **Manual:** remove from `~/.sutra/workspaces.toml`, delete `~/.sutra/josh/`.
- **Structural:** in `sutra_add_root`, refuse to register a workspace whose root is an ancestor of an already-registered workspace (or refuse to register a child of one — pick one direction). This prevents recurrence.

### 5. Fix smriti event fan-out

`src/daemon.rs::poll_smriti_events` (and its callers) should route each event to the **most specific** matching workspace, not every ancestor. Otherwise any workspace whose root contains another workspace amplifies its watcher load by N. This is independent of #4 — even if `josh` is removed, `manas` still contains `sutra`, `chitta`, `kosha`, `manas-cli`, and `yojana`.

### 6. Surface scheduler health to MCP clients

The MCP `is_stale` flag confused the user precisely because the index *is* stale but the answers it gives are still useful. Two improvements worth considering:

- A separate `scheduler_last_tick` field in the freshness envelope, distinct from `as_of` (the snapshot). If the scheduler hasn't ticked in >2× threshold, surface it.
- Health endpoint should report scheduler liveness, not just per-workspace last_parse counts.

## What the user should do right now

To unblock the running daemon:

```bash
systemctl --user restart sutra
```

This won't prevent recurrence. Fixes #1 and #2 are the minimum required to make recurrence non-fatal. Fix #4 (remove the `josh` workspace) prevents the trigger.

## Related files

| File | Lines | What |
|---|---|---|
| `src/daemon.rs` | 235-296 | `check_stale_workspaces` — the wedged loop |
| `src/daemon.rs` | 73-125, 127-194 | `smriti_watcher_loop`, `flush_debounced` — parallel writer competing for `josh.db` |
| `src/pipeline.rs` | 280-344 | `parse_workspace` — no snapshot on failure |
| `src/pipeline.rs` | 346-424 | `parse_changed_files` — same |
| `src/pipeline.rs` | 462-484 | `record_snapshot` — only called from success paths |
| `src/db.rs` | 138-160 | `Db::open` — `busy_timeout = 5000` |
| `src/db.rs` | 901-912 | `last_parse_time` — returns `None` ⇒ stale |
| `src/mcp.rs` | 207-221 | `freshness` — emits `is_stale` |
| `~/.sutra/workspaces.toml` | — | The pathological `josh` registration |
