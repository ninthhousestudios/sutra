# Release Gate Synthesis — sutra v0.7.2 @ 8a70710

**Date:** 2026-06-09
**Scope:** all of HEAD, release gate (pre-tag)
**Verdict:** **HOLD FOR FIXES** — build/tests green (738/738), but three load-bearing mechanisms don't deliver what their contracts state, and the test suite asserts shapes rather than behavior, so green tests don't contradict the hold.

**Sources** (read these for full detail; this file is the entry point):
- [2026-06-09-release-gate-review.md](2026-06-09-release-gate-review.md) — code review pass: 4 High, 4 Medium, 3 Low
- [2026-06-09-release-gate-architecture.md](2026-06-09-release-gate-architecture.md) — deepening pass: 7 candidates

## Convergent root causes

The two passes ran blind to each other and hit the same diseases from different angles. Where they converge is the highest-leverage work:

**1. The self-index is not trustworthy, and everything sits on it.**
The review pass found two High correctness holes in Layer 0: the Rust parser never extracts method-call names (`field_identifier`), so `sutra_impact`/`refs`/`calls`/`dead` are systematically wrong for idiomatic Rust (`Db::upsert_file` → "0 callers, low risk"); and non-atomic file replace + no cross-process parse lock has *already* duplicated every `src/tools/review.rs` symbol in the live index, with no self-heal. The architecture pass independently tripped over the consequence: the pack's "dead cluster" in review.rs was alive (`compute` calls it). **Every single entry** in the review pack's curated dead-code list was a false positive manufactured by these two bugs. Until they're fixed, sutra's factual claims — the product — mislead the LLM agents consuming them, and any post-fix verification against the self-index is unreliable.

**2. Enforcement semantics have forked across copies.**
The architecture pass found constraint evaluation implemented four times (review, constraints tool, orient, guard) with waiver matching *already divergent* — guard matches `from || to`, everyone else `from`-only. The review pass found guard's check inverted relative to the README contract: it reads pre-edit index state, so the violating edit passes and the *fixing* edit gets denied. These are one disease: enforcement logic duplicated until it forked. The structural fix (one pure `constraints::check::evaluate` core) makes the divergence class impossible; the guard-timing fix is a deliberate design choice that should be made once, in that core, not patched in one of four copies.

**3. Contracts maintained as prose, divorced from mechanism.**
Server self-description hardcodes v0.2.1 / 21 tools (actual: 0.7.2 / 33) — this very review's pack-builder mis-attributed live bugs to "daemon lag" because of it. `sutra_cochange` always returns empty (git pathspec bug) while health's co-change path works, so two surfaces contradict each other. Review's `[introduced]` label overclaims. README says "28 MCP tools" and promises introduce-time guard blocking. Same consumer hurt each time: an LLM agent with no way to second-guess the tool.

**4. Three arcs, three recompute policies, and scatter as the residue.**
Conventions rebuild only as a side effect of running Review (FCA pipeline trapped in `build_findings`, cognitive 131); components/health/HRR rebuild at parse time; DD constraints ingest lazily at tool time. The measured co-change clumps (review.rs at 60 partners, pipeline.rs at 90, hotspots↔dead at 80%) are the behavioral footprint of horizontal infrastructure re-implemented per arc: health rollup ×3, change-risk gathering ×3, waiver partition ×6.

## Fix order (waves — one commit per wave)

| Wave | Content | Blocking? | Source findings |
|---|---|---|---|
| **0 — Hygiene** | `cargo fmt` (8,465-line drift) + `clippy --fix` sweep (66 auto-fixable of 126) as mechanical commits; add fmt/clippy gates to the pre-commit hook; `.git-blame-ignore-revs` for the fmt commit | Yes (pre-tag) | fmt-clippy-hygiene |
| **1 — Index integrity** | Atomic per-file replace (one tx), `UNIQUE(file_id, qualified_name, start_line)` backstop, cross-process advisory lock, startup integrity sweep to heal existing duplicates | **Yes** | parse-race-duplicate-symbols |
| **2 — Ref precision** | Extract `field_identifier` in call position; reclassify receiver-position identifiers; full reparse; acceptance: `Db::upsert_file` shows real callers, dead-code false-positive rate collapses | **Yes** | rust-refs-miss-method-calls |
| **3 — Cochange** | Answer `sutra_cochange` from the already-correct `commit_files` tables; delete the broken git plumbing; add behavioral (non-emptiness) test; audit other git-backed tool tests | **Yes** | cochange-always-empty, cochange-tests-vacuous |
| **4 — Guard semantics** | HITL design call first: parse proposed content vs. rename to "flag on next touch" (either way: fix-path exemption + README truthing). Decide the canonical waiver-match rule (from-only vs from‖to) here too | **Yes** (README ships false claim) | guard-checks-pre-edit-state; arch candidate 1 (waiver divergence) |
| **5 — Contract batch** | `env!("CARGO_PKG_VERSION")` + generated tool roster; `all_constraints` partitions into (valid, errors) surfaced as findings instead of silent wholesale failure; `[introduced]` baseline from merge-base or honest rename; README tool count | Follow-up | server-contract-drift, rules-error-silently-disables-constraints, review-introduced-label-overclaims |
| **6 — Slop** | Delete/wire `dead_symbol_ratio_by_file`; hoist duplicated `SKIP_DIRS` | Follow-up | dead-symbol-ratio-unused, skip-dirs-duplicated-const |
| **A — Architectural deepening** | Candidates 1→2→3→4→5→6/7 from the architecture pass (constraint core, convention rebuild → parse pipeline, waiver seam, health rollup, change signals, components split, tool-handler seam). Needs grilling before implementation; candidate 1 should land before or with Wave 4's semantic decision | Post-gate arc | architecture pass |

Sequencing rationale: Wave 1 before Wave 2 because verifying the ref fix against a corrupted index proves nothing. Waves 1–4 are the hold-blockers; the tag can ship after Wave 4 with 5/6 queued. Wave A is its own arc — candidates 1+2 dismantle `build_findings` and should be grilled as a unit.

## Process feedback (for the vidhi-release-review skill)

- The pack-builder's dead-code "credible candidates" heuristic needs new filter entries; better, post-Wave-2 the false-positive mechanisms (method-call invisibility, receiver-position skips, format!-string interpolation, duplicate-row twins) mostly disappear at the source.
- `get_info()` version drift sent the pack-builder down a "deployed daemon lag" path; Wave 5's generated self-description fixes the root.
