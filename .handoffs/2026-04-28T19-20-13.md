# sutra — handoff

Date: 2026-04-28 (post v0.1.5 implementation)

## status

sutra v0.1.5 with 107 tests passing, zero clippy warnings. Binary at `~/.cargo/bin/sutra`. MCP server registered in `~/.claude.json` (stdio mode). Guard hooks installed in `~/.claude/settings.json`. `~/.claude/CLAUDE.md` updated to route through sutra instead of qartez.

### v0.1.5 (this session)

All 5 items from previous handoff implemented:

1. **InsertSymbolParams struct** — Replaced 13-param `insert_symbol` with a params struct. Updated all 27 call sites across tests.
2. **sutra_add_root MCP tool** — Auto-register + async parse a workspace. Agent calls it at session start via CLAUDE.md instruction.
3. **PageRank population** — Power iteration on file dependency graph (damping=0.85, epsilon=1e-6). Distributed to symbols by incoming ref weight. Results: lib.rs 0.255, error.rs 0.152.
4. **Guard hooks (routing + modification)** — `sutra-guard` binary. Routing guard denies Glob/Grep when sutra index exists. Modification guard blocks edits to load-bearing files (pagerank >= 0.05 or blast_radius >= 10) until `sutra_impact` is called (ack protocol with 600s TTL). Fail-open design.
5. **Incremental rollup recompute** — Only recomputes changed files + depth-1 neighbors. Batch SQL queries eliminated N+1 patterns (~24% speedup).

## next steps

### v0.2.0 candidates

- **Incremental parse** — Only re-parse files changed since last parse (by mtime or git diff). Currently full-reparse every time.
- **Cross-workspace refs** — Resolve symbols across workspace boundaries (e.g., a library used by multiple projects).
- **sutra_refactor_plan** — Generate ordered refactor steps with safety annotations, like qartez has.
- **Test gap analysis** — Identify symbols with high blast radius but no test coverage.
- **Watch mode** — File watcher that triggers incremental re-parse on save.

### decided against

- **HTTP daemon** — Stdio gives per-project scoping for free. See chitta memory `019dd565-65f4`.
- **Local variable extraction** — Would balloon symbol table ~10x for refs useless to cross-file tools.
- **External crate indexing** — Big scope expansion for marginal gain.

### deferred from reviews (do later)

- **Connection pool / read-write split** — Over-engineering for current scale.
- **Rollups in SQL** — In-Rust computation works. Revisit at 10K+ files.

## related memories

- Session summary: `019dd565-d8f2`
- No-HTTP decision: `019dd565-65f4`
- Prior session summary (v0.1.1 review cleanup): `019dd1c1-d9e9`
