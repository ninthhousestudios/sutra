# Review Synthesis: sutra v0.2.0 checkpoint

**Date:** 2026-05-08
**Scope:** All of HEAD (8b0fe30, main branch)
**Verdict:** Continue with adjustments
**Source files:**
- [Architecture pass](2026-05-08-checkpoint-architecture.md) — 6 deepening candidates
- [Code review](2026-05-08-checkpoint-review.md) — 5 findings (0 critical, 3 medium, 2 low)

## Convergent root causes

The two passes hit the same three diseases from different angles. These are the highest-leverage fixes because addressing them resolves clusters of findings simultaneously.

### 1. pipeline.rs is doing too many jobs (Architecture #1, #3 + Review F1, F4)

The architecture pass identifies pipeline.rs as containing four distinct responsibilities (file walking, parse-and-persist, reference resolution, graph analysis) with duplicated orchestration between `parse_workspace` and `parse_changed_files`. The code review found two concrete symptoms: F1 (duplicate `all_symbol_file_map()` call — the same query running twice because the shared sequence isn't factored out) and F4 (daemon wrapping async parse in `spawn_blocking + block_on` — partly because the pipeline interface doesn't make the async boundary clear).

The architecture pass's candidate #3 (extract a `graph` module) is a subset of #1 — pulling PageRank, BFS, and rollup computation into their own module is the first step toward the broader pipeline decomposition. F1 goes away automatically when the post-parse sequence is unified.

### 2. mcp.rs contains business logic that belongs elsewhere (Architecture #2, #5 + Review F2, F3)

The architecture pass flags `mcp.rs` as a 1039-line pass-through where `sutra_add_root` and `sutra_status` are exceptions — they contain real business logic (workspace registration, daemon probing). The code review found two error-handling bugs in exactly that business logic: F2 (daemon errors silently swallowed, masking broken daemons) and F3 (workspace persistence failure silently swallowed on the fallback path).

Both passes independently identify the workspace registration duplication (architecture #5, review slop #7). These bugs exist *because* the logic is duplicated — one copy propagates errors, the other doesn't.

Extracting workspace registration into a shared method (architecture #5) fixes the duplication and forces consistent error handling. Moving `add_root` and `status` out of `mcp.rs` (architecture #2) prevents future business logic from accumulating in the dispatch shell.

### 3. db.rs interface grows linearly with features (Architecture #4 + Review F5)

The architecture pass diagnoses `db.rs` as shallow (45+ thin SQL wrappers, churn=18) with callers forced to compose low-level queries. The code review finds the most egregious symptom: F5 (`insert_snapshot` with 9 positional `i64` arguments). Both point at the same disease — `db.rs` adds a method per feature with no domain intelligence at the interface.

## Fix order

### Wave A — Quick correctness + hygiene (no structural changes)

| Item | Source | Fix | Time |
|------|--------|-----|------|
| `cargo fmt` | Review slop #1 | Run once, standalone commit | 2 min |
| F1: duplicate `all_symbol_file_map` | Review | Hoist call above branch | 5 min |
| F3: swallowed error in `sutra_status` | Review | Add `map_err` or log | 5 min |
| Clippy collapsible ifs | Review slop #2 | `cargo clippy --fix` in winnow.rs | 5 min |

**These are blocking.** Do them before any structural work so review fix commits have clean diffs.

### Wave B — Error handling + workspace consolidation

| Item | Source | Fix | Time |
|------|--------|-----|------|
| F2: `try_daemon_register` error types | Review | Richer error enum, fall back only on ConnectFailed | 30 min |
| Architecture #5: workspace registration | Architecture | Extract shared `register_workspace` method | 30 min |

F2 and #5 should be done together — they touch the same code paths in mcp.rs. Doing #5 first eliminates the duplication that caused F3.

### Wave C — Structural extraction (the big refactor)

| Item | Source | Fix | Time |
|------|--------|-----|------|
| Architecture #3: extract `graph` module | Architecture | Pull PageRank, BFS, rollups from pipeline.rs | 2-3 hr |
| Architecture #1: unify pipeline orchestration | Architecture | Shared post-parse sequence, F1 resolved | 1-2 hr |
| Architecture #2: MCP dispatch thinning | Architecture | Move arg structs to tool modules | 1-2 hr |
| F4: spawn_blocking pattern | Review | Replace with tokio::spawn in daemon.rs | 30 min |
| F5: insert_snapshot params struct | Review | SnapshotParams struct | 15 min |

Wave C corresponds to the existing refactor plan's PRs 2-4. F4 and F5 slot into those PRs naturally.

### Wave D — Follow-up deepening (HITL — needs grilling before implementation)

| Item | Source |
|------|--------|
| Architecture #4: db.rs domain methods | Architecture |
| Architecture #6: winnow.rs predicate composition | Architecture |

These change module interfaces, not just internal structure. They need design discussion before implementation.

## Observations for vidhi/review/1 (skill validation)

1. **Pack template is portable.** Same structure as the chitta run, no per-project tweaking needed. The SKILL.md was sufficient to follow cold.
2. **Analysis tier needs enabling.** The SKILL.md doesn't mention calling `sutra_tools enable=["analysis"]` before code-intel tools. Add this to step 3.
3. **sutra_dead filtering gap.** Despite sutra/13, integration test files (`tests/*.rs`) and `#[cfg(test)]` module helpers still appear. Three sutra-side fixes available (file the bug), plus pack-side filtering for MCP-registered handlers.
4. **Non-overlapping findings confirmed.** Architecture found structural drift (6 candidates); review found correctness and contract issues (5 findings). Only workspace registration duplication appeared in both — and they surfaced it from different angles (architecture: "this is duplicated" vs review: "the duplicate has a bug"). This validates the two-pass design.
5. **Subagent prompts worked without per-project tweaking.** Both produced well-structured output following the requested format. The reviewer correctly verified all three "likely dead code" items from the pack SUMMARY and confirmed them as false positives (slop #3-5).
6. **Review file format drift.** The reviewer-prompt.md asks for YAML blocks but doesn't specify that each block should be in its own fenced code block. The architecture skill doesn't specify a finding schema at all — it just says "numbered list." Both worked, but the formats aren't directly reconcilable for automated processing. Not a problem at this scale.
