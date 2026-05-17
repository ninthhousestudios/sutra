# sutra

[![License: AGPL v3](https://img.shields.io/badge/License-AGPL_v3-blue.svg)](https://www.gnu.org/licenses/agpl-3.0)

Code intelligence for [manas](https://github.com/ninthhousestudios/manas) — per-workspace structural index via tree-sitter, served as an MCP server.

sutra parses your codebase into a SQLite index of symbols, references, imports, and file-level dependency graphs, then exposes that index through 14 MCP tools that AI coding agents can call.

## Install

```bash
cargo install --path .
```

## Quick start

```bash
# Register a workspace
sutra workspaces add myproject /path/to/project rust

# Parse it
sutra parse myproject

# Start the MCP server (stdio for agent integration)
sutra serve --stdio

# Or as an HTTP daemon (default, shared across clients)
sutra serve
```

## MCP tools

### Core (always available)

| Tool | Description |
|------|-------------|
| `sutra_health` | Per-workspace file/symbol counts, parse errors, staleness |
| `sutra_map` | Project file skeleton ranked by importance |
| `sutra_outline` | File symbol table of contents |
| `sutra_find` | Jump to a symbol definition by name (exact + FTS5 fuzzy) |
| `sutra_grep` | Search indexed symbols by name pattern |
| `sutra_read` | Read a symbol's source code with line numbers |
| `sutra_impact` | Blast radius analysis (direct callers, BFS depth-3, risk level) |
| `sutra_deps` | File-level import dependency graph |
| `sutra_parse` | Trigger a workspace reparse |
| `sutra_tools` | Enable/disable tool tiers |

### Analysis (enable via `sutra_tools`)

| Tool | Description |
|------|-------------|
| `sutra_refs` | All usages of a symbol across the codebase |
| `sutra_calls` | Call hierarchy (callers/callees, BFS to depth) |
| `sutra_diff_impact` | Blast radius of a git diff |
| `sutra_cochange` | Files that historically change together |

## MCP configuration

### Claude Code (`~/.claude/settings.json`)

```json
{
  "mcpServers": {
    "sutra": {
      "command": "/home/you/.cargo/bin/sutra",
      "args": ["serve", "--stdio"]
    }
  }
}
```

### Gemini CLI (`~/.gemini/settings.json`)

```json
{
  "mcpServers": {
    "sutra": {
      "command": "/home/you/.cargo/bin/sutra",
      "args": ["serve", "--stdio"]
    }
  }
}
```

### OpenCode (`~/.config/opencode/opencode.json`)

```json
{
  "mcp": {
    "sutra": {
      "command": ["/home/you/.cargo/bin/sutra", "serve", "--stdio"],
      "enabled": true,
      "type": "local"
    }
  }
}
```

## Languages

- **Rust** — full support (functions, structs, enums, traits, impls, methods, modules, consts, macros)
- **Dart** — full support (classes, methods, functions, enums, mixins, extensions, type aliases)

## Configuration

Environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `SUTRA_DB_DIR` | `~/.sutra/` | Database directory |
| `SUTRA_WORKSPACES` | `~/.sutra/workspaces.toml` | Workspace registry |
| `SUTRA_LISTEN_ADDR` | `127.0.0.1:3201` | HTTP listen address |
| `SUTRA_PARSE_PARALLELISM` | CPU count | Max parallel parse workers |
| `SUTRA_STALE_THRESHOLD_SEC` | `600` | Seconds before an index snapshot is marked stale |
| `SUTRA_WATCH_POLL_SEC` | `2` | How often the daemon polls smriti for FS events |
| `SUTRA_WATCH_DEBOUNCE_SEC` | `3` | Quiescent window before flushing a workspace's debounced events |
| `SUTRA_PARSE_TIMEOUT_SEC` | `60` | Max wall-clock for a single workspace reparse before it is aborted |
| `SUTRA_LOG_LEVEL` | `info` | Tracing filter when `RUST_LOG` is unset |

## Daemon (HTTP mode)

`sutra serve` (HTTP) runs three concurrent loops on the same tokio runtime:

- **Scheduler** — every `STALE_THRESHOLD_SEC / 2` it visits each registered workspace, checks the latest snapshot's age, and dispatches a reparse for any that exceed the threshold. Each reparse runs as a detached task wrapped in `PARSE_TIMEOUT_SEC`, so one slow or hung workspace cannot stall ticks for the rest of the fleet.
- **Smriti watcher** — polls the [smriti](https://github.com/ninthhousestudios/manas) FS-event log every `WATCH_POLL_SEC`, fans events out per workspace, and after `WATCH_DEBOUNCE_SEC` of quiet calls `parse_changed_files` to do an incremental update (deleted files cascade-pruned; changed files re-parsed; resolution and rollups recomputed).
- **MCP/HTTP server** — serves tool calls against the snapshot the other two loops maintain.

A per-workspace `tokio::Mutex` guards the index db: the scheduler `try_lock`s and skips this tick if a parse is already in flight; the watcher `.await`s the lock so its buffered events are never dropped. Two parses never run concurrently against the same db.

**Workspace registration** rejects roots that overlap an existing workspace in either direction (ancestor or descendant). This prevents the smriti event fan-out from routing the same file change to two workspaces and racing their reparses against each other.

## Architecture

```
                       ┌───────────────────────────┐
                       │  smriti FS event log      │
                       └────────────┬──────────────┘
                                    │ poll + debounce
                                    ▼
workspace files ──┐         ┌─────────────────┐
                  ├───►  tree-sitter ──> symbols, refs, imports
  scheduler tick ─┘         └────────┬────────┘
                                     ▼
                          resolver ──> resolved refs (local + module + direct imports)
                                     │
                                     ▼
                          SQLite (WAL) ──> fan_in, blast_radius rollups, snapshots
                                     │
                                     ▼
                          MCP server ──> 14 tools with freshness envelopes
```

Every tool response includes a freshness envelope:

- `as_of` — timestamp of the latest snapshot for this workspace
- `is_stale` — whether `as_of` exceeds `STALE_THRESHOLD_SEC`
- `scheduler_last_tick_age_sec` — seconds since the scheduler last ticked (`null` in stdio mode)
- `scheduler_alive` — `true` iff the scheduler ticked within `2 × STALE_THRESHOLD_SEC`

The last two distinguish "this workspace is quietly old" from "the scheduler itself has wedged" — they answer different questions and you usually want both.

Unresolved references are reported honestly — the v0.1 resolver handles ~60-70% of references (local bindings, module-level items, direct imports).

