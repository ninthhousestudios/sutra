# sutra — handoff

Date: 2026-04-28 (post resolver improvement)

## status

sutra v0.1.1 with 107 tests passing, zero clippy warnings. Binary at `~/.cargo/bin/sutra` (rebuilt this session). Sutra is now registered as an MCP server in Claude Code (`~/.claude.json`, stdio mode with `--stdio` flag). Smriti also added to MCP config (HTTP on 127.0.0.1:7333).

### resolver improvement (this session, uncommitted)

Kind-aware resolution: resolver now uses `context_kind` to filter symbol matches (TypeUse -> types, Call -> callables). Import and FieldAccess refs are skipped (not counted as unresolved). Brace-grouped Rust imports (`use std::{A, B}`) expanded into individual entries. Dart parser now captures `type_identifier` nodes.

Resolution rate on sutra's own codebase: **21% of resolvable refs** (1821 of 8341), with 1789 refs properly skipped. Precision improved — TypeUse refs no longer resolve to functions, Call refs no longer resolve to structs. The remaining ~79% unresolved are mostly local variables/params (72%) and external stdlib symbols (28%) — both out of scope for a structural index.

Files changed: `resolver.rs`, `db.rs`, `pipeline.rs`, `parser/rust.rs`, `parser/dart.rs`, `main.rs`, `tools/parse.rs`, plus test files.

## next steps

### v0.1.5 candidates (pick any)

- **`sutra_add_root` MCP tool** — Auto-register + parse a workspace when the agent first connects. Like qartez's `qartez_add_root`. The agent calls it with cwd at session start, sutra registers the workspace and parses it. Makes sutra zero-setup for new repos. CLAUDE.md would instruct the agent to call it.
- **PageRank population** — Schema has nullable `pagerank` columns, always NULL. Compute from the ref graph and populate. Low effort, improves `sutra_map` ranking quality immediately.
- **Incremental rollup recompute** — Currently full-recompute every parse. Only recompute changed files + depth-1 dependents. Optimization, not urgent.

### decided against

- **HTTP daemon** — Stdio gives per-project scoping for free. See chitta memory `019dd565-65f4`.
- **Local variable extraction** — Would balloon symbol table ~10x for refs that are useless to cross-file tools (impact, refs, calls). Resolution rate would jump to ~70% but none of those refs are actionable.
- **External crate indexing** — Parsing `~/.cargo/registry/src/` is a big scope expansion. A hardcoded stdlib list (~50 symbols) would help but resolved refs still point outside the workspace.

### deferred from reviews (do later)

- **Connection pool / read-write split** — Over-engineering for current scale.
- **Rollups in SQL** — In-Rust computation works. Revisit at 10K+ files.
- **InsertSymbolParams struct** — Replaces 13-param `insert_symbol`. Nice-to-have.

## housekeeping

- Changes from this session are **uncommitted** — commit the resolver improvement.
- Stale worktree branches may still exist — `git worktree list` to check.

## related memories

- Session summary: `019dd565-d8f2`
- No-HTTP decision: `019dd565-65f4`
- FTS5 bug observation: `019dd565-51c9`
- Prior session summary (v0.1.1 review cleanup): `019dd1c1-d9e9`
