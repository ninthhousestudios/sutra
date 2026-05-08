# Sutra watcher daemon — PRD

Date: 2026-05-07
Task: sutra/1

## Problem

Sutra's HTTP daemon uses timer-based staleness checks to keep workspace indexes
fresh. It reparses entire workspaces on a fixed interval regardless of whether
anything changed. This is wasteful for idle workspaces and slow to react to
actual changes — a file save takes up to `stale_threshold_sec / 2` to appear in
the index.

## Solution

The sutra daemon watches smriti's events table for file changes and
incrementally reparses only the affected files. Stdio sessions check the daemon
on startup and become read-only when the daemon is maintaining the index.

## Architecture

### Two operating modes

- **HTTP daemon mode** (`sutra serve-http`): long-running process, watches
  smriti for file change events, keeps all registered workspace indexes warm.
  Runs as a systemd user service.
- **Stdio mode** (`sutra serve-stdio`): per-session, project-scoped. Checks
  the daemon on startup. If daemon is alive, opens DB read-only. If not, falls
  back to local parse with timer-based staleness (current behavior).

### Smriti integration

The daemon reads smriti's `index.db` directly via SQLite (read-only, WAL mode
allows concurrent readers). It polls `events_since(cursor, limit)` every 2
seconds and consumes the event stream.

This avoids sutra being an MCP client to smriti. Both are manas components on
the same machine, same user. The events table schema is stable and controlled.

**Smriti DB location**: `SUTRA_SMRITI_DB` env var, defaulting to
`~/.smriti/index.db`.

**Graceful degradation**: if smriti's DB is not available (doesn't exist, locked,
corrupt), the daemon falls back to timer-based staleness polling (current
`check_stale_workspaces` behavior). If the DB reappears, the daemon reconnects
on the next poll cycle.

### Event processing

```
loop (every 2s):
  events = smriti.events_since(cursor, 1000)
  if !events.cursor_valid:
    log warning, reset cursor to 0, full reparse all workspaces
    continue

  cursor = events.next_cursor
  persist cursor to ~/.sutra/smriti_cursor

  for event in events:
    if not source_extension(event.path): skip
    ws = find_workspace_for_path(event.path)
    if ws is None: skip
    debounce_buffer[ws].push(event)

  for (ws, buffer) in debounce_buffers where buffer.ready(3s):
    changed = buffer.drain_modified()
    deleted = buffer.drain_deleted()
    spawn_blocking(parse_changed_files(ws, changed, deleted))
```

**Cursor persistence**: stored in `~/.sutra/smriti_cursor` (single i64). Read on
startup, written after each successful poll. On first run or missing file,
starts from 0 (triggers full reparse).

**Cursor invalidation**: when smriti has pruned events past the cursor (default
24h retention), `cursor_valid` returns false. The daemon logs a warning, resets
to 0, and does a full reparse of all workspaces.

### Event-to-workspace mapping

Smriti events contain absolute file paths. The daemon:

1. Filters by source file extension (`.rs`, `.dart` — from workspace language
   config)
2. Matches each path to a registered workspace by checking if the path starts
   with the workspace's `root`
3. Groups events per workspace into debounce buffers
4. Events matching no workspace are ignored

### Debouncing

Per-workspace debounce buffer with a 3-second quiet window. After the first
event for a workspace, the buffer collects for 3 more seconds before firing.
This collapses bursts from `git checkout`, `cargo build`, or IDE batch saves
into a single reparse.

Configurable via `SUTRA_WATCH_DEBOUNCE_SEC` (default `3`).

### Incremental parse pipeline

New function alongside existing `parse_workspace`:

```rust
pub async fn parse_changed_files(
    workspace: &WorkspaceEntry,
    db: &Db,
    config: &Config,
    changed: Vec<PathBuf>,   // created or modified
    deleted: Vec<PathBuf>,   // removed
) -> Result<ParseSnapshot>
```

Steps:
1. **Delete**: for each deleted path, remove the file row + symbols + refs from
   DB. Collect deleted symbol IDs.
2. **Parse**: for each changed path, run `parse_single_file`. Collect file IDs
   needing resolution.
3. **Cascade**: find files referencing deleted symbols
   (`db.find_files_referencing_symbols`), add to dirty set.
4. **Resolve**: run `resolve_file_refs` for each file in the dirty set.
5. **Rollups**: `build_file_adjacency` + `compute_rollups_with_graph` with the
   dirty set.
6. **PageRank**: full recomputation (`compute_pagerank`). PageRank is global and
   iterative; incremental PageRank is complex and the full computation is cheap
   relative to parsing.

File renames are treated as delete + create. No special-case logic.

### REST endpoints

Added to the existing axum router alongside `/mcp`:

| Endpoint | Method | Purpose |
|---|---|---|
| `/health` | GET | `manas health` integration. Returns 200 + `{"status": "ok"}` (same pattern as yojana). |
| `/status` | GET | Rich daemon status for stdio sessions and debugging. Returns workspace list with last-parse times, staleness, file/symbol counts, smriti connection status. |
| `/workspaces` | POST | Register a new workspace. Body: `{"root": "/path", "languages": ["rust"]}`. Daemon starts watching it. Triggers initial parse if not already parsed. |

### `sutra_status` MCP tool

Replaces `sutra_add_root` as the session-start call. The agent's CLAUDE.md
instruction changes from "call `sutra_add_root`" to "call `sutra_status`."

Behavior:
1. Check if daemon is alive at `SUTRA_LISTEN_ADDR`
2. **Daemon alive**: `POST /workspaces` to register, then `GET /status` for
   workspace info. Block up to 10s if daemon is parsing the workspace for the
   first time. Return status.
3. **Daemon not alive**: fall back to local mode. Register workspace locally,
   parse if needed (same as current `sutra_add_root`), return status.

Response shape:
```json
{
  "workspace": "sutra",
  "root": "/home/josh/soft/manas/sutra",
  "mode": "daemon",
  "status": "ready",
  "last_parse": "2026-05-07T12:01:08Z",
  "files": 40,
  "symbols": 325,
  "smriti_connected": true,
  "is_stale": false
}
```

- `mode`: `"daemon"` or `"local"`
- `status`: `"ready"` | `"parsing"` | `"empty"`
- `smriti_connected`: whether the daemon is getting real-time events (daemon
  mode only)

`sutra_add_root` remains as the explicit "force reparse now" tool. It is no
longer the session-start call.

### Concurrent access

- **Daemon alive**: stdio sessions open workspace DBs read-only. The daemon is
  the sole writer. SQLite WAL mode allows concurrent readers.
- **Daemon not alive**: stdio sessions write locally (current behavior).
- **Stale checker**: runs alongside the smriti watcher as a safety net. Skips
  workspaces that the smriti watcher has refreshed within the stale threshold.

### Daemon architecture

```
Daemon
├── spawn_smriti_watcher()     — poll loop + debounce + incremental reparse
├── spawn_scheduler()          — timer-based fallback, safety net
├── axum router
│   ├── GET /health
│   ├── GET /status
│   ├── POST /workspaces
│   └── /mcp
└── smriti_cursor: persisted i64
```

On startup:
1. Load `workspaces.toml` (existing behavior)
2. Read `~/.sutra/smriti_cursor` (or start from 0)
3. Try to open smriti's DB read-only
4. If smriti available → start smriti watcher + stale checker (safety net)
5. If smriti unavailable → stale checker only (current behavior), retry smriti
   connection on each poll cycle

### manas-cli integration

- New config: `MANAS_SUTRA_URL` defaulting to `http://127.0.0.1:3201`
- Add `sutra` to the health check loop in `manas-cli/src/cmd/health.rs`
- Add `sutra_url` field to `ManasConfig`

## Configuration

| Env var | Default | Description |
|---|---|---|
| `SUTRA_SMRITI_DB` | `~/.smriti/index.db` | Path to smriti's SQLite DB |
| `SUTRA_WATCH_POLL_SEC` | `2` | How often to poll smriti events |
| `SUTRA_WATCH_DEBOUNCE_SEC` | `3` | Per-workspace quiet window before reparse |
| `SUTRA_DB_DIR` | `~/.sutra/` | Existing — where workspace DBs live |
| `SUTRA_LISTEN_ADDR` | `127.0.0.1:3201` | Existing — HTTP bind address |
| `SUTRA_STALE_THRESHOLD_SEC` | `3600` | Existing — timer-based staleness |
| `SUTRA_PARSE_PARALLELISM` | num CPUs | Existing — parse thread count |
| `MANAS_SUTRA_URL` | `http://127.0.0.1:3201` | manas-cli config for health checks |
