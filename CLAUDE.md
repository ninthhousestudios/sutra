## Code exploration

This workspace is indexed by sutra (MCP code intelligence). For code exploration:
- Use `sutra_map` instead of `find`/`ls` to discover files
- Use `sutra_outline` instead of reading whole files to get symbol tables of contents
- Use `sutra_grep` / `sutra_find` instead of shell `grep` for symbol search
- Use `sutra_read` to read specific symbols by name instead of reading full files

For convention system work, read `docs/conventions-map.md` first — it's a compressed architecture reference that replaces broad exploration.

## Agent skills

### Issue tracker

Yojana (local MCP task graph). See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary (needs-triage, needs-info, ready-for-agent, ready-for-human, wontfix). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context layout. See `docs/agents/domain.md`.
