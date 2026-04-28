# sutra

Code intelligence for [manas](https://github.com/josh/manas) — per-workspace structural index via tree-sitter, served as an MCP server.

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
| `SUTRA_STALE_THRESHOLD_SEC` | `600` | Seconds before index is marked stale |

## Architecture

```
workspace files
      |
  tree-sitter ──> symbols, refs, imports
      |
  resolver ──> resolved refs (local + module + direct imports)
      |
  SQLite (WAL) ──> fan_in, blast_radius rollups
      |
  MCP server ──> 14 tools with freshness envelopes
```

Every tool response includes `as_of` (last parse timestamp) and `is_stale` (whether the index exceeds the staleness threshold). Unresolved references are reported honestly — the v0.1 resolver handles ~60-70% of references (local bindings, module-level items, direct imports).

## License

MPL-2.0
