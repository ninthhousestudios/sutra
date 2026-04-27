# sutra — handoff

Date: 2026-04-27 (post-planning session)

## status

Implementation plan complete and pre-mortem'd. No code yet. All blockers resolved.

- **Plan:** `.agents/plans/2026-04-27-sutra-v01-implementation.md` — 10 issues, 5 waves, ~5,100-5,500 LOC
- **Pre-mortem:** `.agents/council/2026-04-27-pre-mortem-sutra-v01.md` — WARN verdict, all 10 amendments applied
- **Design sketch:** `docs/sutra-sketch.md` — 686 lines, design of record

## what to pick up

### start implementing — Issue 1 (project scaffold)

No blockers remain. The tree-sitter compatibility spike passed (tree-sitter 0.25.10 + tree-sitter-rust 0.24.2 + tree-sitter-dart 0.2.0). All design decisions are resolved.

Issue 1 creates: Cargo.toml, src/main.rs (clap CLI), src/lib.rs (all mod declarations), src/config.rs, src/error.rs, and stub files for every other module. Reference sangha's `main.rs`, `config.rs`, `error.rs` for the exact pattern.

After Issue 1, Wave 2 opens: Issues 2 (db), 3 (workspace), 4 (Rust parser) can run in parallel.

### key decisions already made

- **Standalone crate** (not Cargo workspace)
- **HTTP daemon** (one instance, all workspaces; stdio as `--stdio` fallback)
- **tree-sitter versions:** `tree-sitter = "0.25"`, `tree-sitter-rust = "0.24"`, `tree-sitter-dart = "0.2"`
- **Resolver v0.1 scope:** local bindings + function params + module-level items + direct imports only (~60-70% resolution rate). Calibrate unresolved rate against sutra's own codebase.

### pre-mortem amendments baked into the plan

The plan already includes all fixes from the pre-mortem. Key ones to remember:
- Cross-file dirty marking in incremental reparse (Issue 5)
- `sutra_tools` meta-tool for analysis tier gate (Issue 6)
- `sutra_parse` MCP tool for agent-triggered reparse (Issue 6)
- FTS5 manual sync, not triggers (Issue 2)
- File size cap 100k lines, cycle detection in resolver (Issue 5)
- `sutra_read` returns stale warning on deleted files, not crash (Issue 6)

## related memories

- Session summary: `019dd066-6a22`
- Plan observation: `019dd056-1d90`
- Transport decision: `019dd058-87f3`
- Pre-mortem observation: `019dd05c-7544`
- tree-sitter spike: `019dd065-8ed0`
- tree-sitter ABI gotcha: `019dd066-0a6c`
- Design decisions (from prior session): `019dce02-fa04`, `019dce0f-0673`

## related docs

- `docs/sutra-sketch.md` — design of record
- `.agents/plans/2026-04-27-sutra-v01-implementation.md` — implementation plan
- `.agents/council/2026-04-27-pre-mortem-sutra-v01.md` — pre-mortem report
- `/home/josh/soft/manas/docs/manas-architecture.md` — system context
