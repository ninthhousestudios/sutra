# Handoff — sutra

## In progress

**sutra/1 — watcher daemon PRD**: complete. PRD at `docs/plans/watcher-daemon-prd.md`.
Decomposed into 6 slices (sutra/4 through sutra/9), all needs-triage.

## What to pick up next

### Triage the watcher slices

All six slices need triage. Three can start in parallel:

- **sutra/4** — incremental parse pipeline (`parse_changed_files` in `pipeline.rs`)
- **sutra/5** — REST endpoints (`/health`, `/status`, `/workspaces`) + `install-services` CLI command
- **sutra/6** — smriti event reader + cursor persistence (new module, direct SQLite read of smriti's `index.db`)

Then:
- **sutra/7** — smriti watcher loop (blocked by 4, 6)
- **sutra/8** — `sutra_status` MCP tool (blocked by 5)
- **sutra/9** — manas-cli health integration (blocked by 5)

### Other backlog

- **sutra/3** — `--explain` flag on analysis tools (from qi comparison session)
- Dart language gaps (else-if chain handling, unreachable-file exclusion patterns)
- New tools from qartez survey (context, test_gaps, clones)

## Context

- vidhi-init was run: `CLAUDE.md` + `docs/agents/` created with yojana tracker, default triage labels, single-context domain docs
- No `CONTEXT.md` yet — will be created lazily during implementation when domain terms are resolved
- 165 tests passing, clippy has 1 pre-existing `type_complexity` warning
