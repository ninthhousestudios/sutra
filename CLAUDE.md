## Code exploration

This workspace is indexed by sutra (MCP code intelligence). For code exploration:
- Use `sutra_map` instead of `find`/`ls` to discover files
- Use `sutra_outline` instead of reading whole files to get symbol tables of contents
- Use `sutra_grep` / `sutra_find` instead of shell `grep` for symbol search
- Use `sutra_read` to read specific symbols by name instead of reading full files

When exploring an unfamiliar area, start with `sutra_explore(query, workspace)` — it replaces iterative map/outline/grep cycles with a single ranked result set and strategy hint.

For convention system work, read `docs/conventions-map.md` first — it's a compressed architecture reference that replaces broad exploration.
For constraint system work, read `docs/constraints-map.md` first — same idea, covers DD engine, rules parsing, TOML format, and remaining task graph.
For health/similarity system work, read `docs/health-map.md` first — covers the finding model, biomarker tiers, waiver mechanism, pipeline integration, and scoring design.
For lessons system work, the core module is `src/lessons/` — shared sqlite store at `~/.sutra/lessons.db`, anchor matching engine, confidence lifecycle, and contextual surfacing through `sutra_read`/`sutra_impact`/`sutra_orient`.

## Agent skills

### Issue tracker

Yojana (local MCP task graph). See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary (needs-triage, needs-info, ready-for-agent, ready-for-human, wontfix). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout. See `docs/agents/domain.md`.

## Refactor contract discipline

Consolidation refactors here have silently changed observable contracts three
times (lessons ledger L9: violation totals counted post-waiver-partition,
a "complexity unavailable" sentinel replaced by a different predicate,
freshness snapshotted at request start instead of response time). When
extracting shared helpers, diff the *behavioral contract* of every caller —
output field semantics, sentinel values, timing of snapshots, work performed —
not just types and tests.
