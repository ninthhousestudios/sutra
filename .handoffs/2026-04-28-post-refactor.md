# sutra — handoff

Date: 2026-04-28 (post test coverage + refactor)

## status

sutra v0.1.1 with 102 tests passing, zero warnings. Binary at `~/.cargo/bin/sutra`. Two commits this session: test coverage (80295ca) and god function decomposition (ac12896).

Test coverage is now solid across db.rs, calls.rs, resolver.rs, pipeline.rs, and the FromStr impls. FTS5 colon-injection bug fixed.

`parse_workspace` decomposed into `parse_single_file` + `resolve_file_refs` — no longer a god function.

## next steps

### v0.1.5 candidates (pick any)

- **PageRank population** — Schema has nullable `pagerank` columns, always NULL. Compute from the ref graph and populate. Low effort, improves `sutra_map` ranking quality immediately.
- **Resolver improvement** — Currently ~21% resolution on sutra's own codebase. Biggest wins: type_identifier matching (refs to types like `Config` tagged as `type_identifier` by tree-sitter) and cross-file qualified name resolution. Medium effort.
- **Incremental rollup recompute** — Currently full-recompute every parse. Only recompute changed files + depth-1 dependents. Optimization, not urgent.

### decided against

- **HTTP daemon** — Decided this session. Stdio gives per-project scoping for free. HTTP would add routing/auth/systemd complexity for no benefit. See chitta memory `019dd565-65f4`.

### deferred from reviews (do later)

- **Connection pool / read-write split** — Over-engineering for current scale.
- **Rollups in SQL** — In-Rust computation works. Revisit at 10K+ files.
- **InsertSymbolParams struct** — Replaces 13-param `insert_symbol`. Nice-to-have.

## housekeeping

- Stale worktree branches still exist — `git worktree list` to see, `git worktree remove` + `git branch -D` to clean up.
- sutra is NOT registered as an MCP server in Claude Code. When ready to replace qartez: `claude mcp add -t stdio -s user -- sutra sutra serve`.

## related memories

- Session summary: `019dd565-d8f2`
- No-HTTP decision: `019dd565-65f4`
- FTS5 bug observation: `019dd565-51c9`
- Prior session summary (v0.1.1 review cleanup): `019dd1c1-d9e9`
