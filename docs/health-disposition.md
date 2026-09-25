# Health subsystem disposition (sutra/464)

Decided 2026-09-25. Evidence: [ai-failure-modes-evidence.md](ai-failure-modes-evidence.md),
the vidhi/review/5 pilot (82 findings across 4 repos: 17% actionable, 29% wrong;
trend had zero signal), and the back-tests
[sibling-pattern](sibling-pattern-backtest.md) (462),
[dup-exists](dup-exists-backtest.md) (463) and
[swallow-ratchet](swallow-ratchet-backtest.md) (465).

## The test

A signal is kept only if it helps an agent **while writing code**, which means
all three of these hold:

1. **Diff-scoped.** It speaks about the change in hand, not the standing state
   of a file or repo.
2. **Names something.** It points to a function, site or partner file to act on,
   not a number.
3. **Predicts bugs.** It ties to a mode that produces bugs (PAR, DUP, SWALLOW,
   UNWIRED).

The burden of proof is on keeping. "Could be helpful with the right consumer"
is how 13 biomarkers and a scoring algebra accumulated, and the health layer
was sutra's largest bug source (25 of 89 closed bugs). Deletion is reversible
from git history, so "maybe" resolves to delete.

Scores fail test 2 by construction, since a composite throws away the where
and the what. Trend and snapshots fail test 1. The organizational biomarkers
fail test 3: they measure process (who touched a file, how often), not what to
change.

## Disposition

| Surface | Disposition | Reason |
|---|---|---|
| Composite file/component/workspace scores, deductions, categories, saturation constants | **Delete** (473) | No mechanism can use a score. The only conceivable consumer is a gate on a composite of 83%-non-actionable inputs. |
| Trend, health snapshot detail tables, score basis, comparability gates, `health_runs`/validity publication, `sutra_trend` | **Delete** (473) | Exists only to keep scores comparable over time. Zero calls; zero signal on 4 pilot repos; source of the ~20-bug validity/trend chain. |
| `sutra_file_health`, review `health_delta`, on-demand biomarkers (function_hotspot, code_age_volatility, shape change) | **Delete** (473) | Display and attribution surfaces for scores. file_health had 14 calls ever. |
| co_change_scatter, change_entropy, ownership_risk, blast_radius_churn | **Delete** (474) | 0 actionable. They fire on nearly every file and don't filter bulk commits. Ownership means nothing when agents author everything. |
| hidden_coupling biomarker | **Delete** (474); the idea survives | It duplicates review's `behavioral_coupling` (`src/tools/review.rs`), which is already the diff-scoped form: a co-change partner with no static edge that the diff didn't touch. 3 of 11 actionable in the pilot. Noise fixes go to the review path (476). |
| nested_complexity | **Delete** (474) | Mixed results, and it understates the worst code (depth 5 on an 800-line, cognitive-150 function). Per-symbol cognitive dominates it. |
| import_cycle biomarker | **Delete** (474) | Duplicates the cycle constraint enforced by guard/check. Wrong on Rust (counts `mod` edges; 14/15 files). Whether the constraint shares that bug is to be verified in 477. |
| dead_code_ratio | **Delete** (474) | 0 actionable, wrong in every check. A ratio isn't actionable anyway. The resolver bugs move to 477. |
| Component instability | **Delete** (473) | Uninterpretable; cosmetic penalty. |
| `health_findings`, `health_coverage`, `health_waivers` tables | **Delete** (474) | Nothing left to store. Waivers are user data dropped with the feature. |
| Erosion: mass/share/percentiles, snapshot erosion columns, review `erosion_delta` | **Delete** (475) | Accretion has zero bug root causes. The actionable part (a changed function at cognitive ≥ 15) is already in `diff_impact`, read from indexed per-symbol cognitive. |
| **Per-symbol cyclomatic/cognitive/nesting** (parser) | **Keep.** Consumers: `diff_impact`, `sutra_hotspots` | Hotspots #1 was correct on all 4 pilot repos and found a real guard bug (sutra/456). `COGNITIVE_THRESHOLD` moves out of `health::erosion`. |
| **Co-change tables, `cochange_pairs_above_threshold`, `static_file_edges`** | **Keep.** Consumers: review `behavioral_coupling`, component clustering | The one git signal already shaped to the diff. Candidate for additive PAR, which sibling-pattern misses by construction. Fix in 476. |
| **Per-symbol dead-code resolution** (`sutra_dead`) | **Keep.** Future consumer: the orphans mechanism (UNWIRED) | Needs correctness first (477). |
| **Similarity substrate** (HRR vectors, lexical) | **Keep.** Consumer: the dup advisory (469) | `sutra_similar`'s default strip mode is not a duplicate detector (463). That is a separate fix. |
| **Components** | **Keep.** Consumers: boundary constraints, explore ranking (sutra/373) | Not a health-only surface. |
| **Cycle constraint** | **Keep.** Consumer: guard/check | Unchanged. |
| **`snapshots` table** (the parse record) | **Keep** | Drives `last_parse_time`/`last_parse_info` (REST status, MCP needs-parse, freshness drift). Only health columns and detail tables go. |

Not part of the health subsystem despite the names: `src/tools/health.rs`
(workspace status tool) and `src/tools/scoring.rs` (hotspots/pr_risk math).

## Rejected alternative: freeze

Leave the code, stop wiring and maintaining it. Cheaper today, but frozen code
stays in the graph as debris that misleads the next agent (DEBRIS mode). It
keeps migrations and tables, and it keeps breaking whenever parse or freshness
changes (sutra/412–443 were all freshness interactions). Freezing was
considered per surface, and no surface was judged risky enough to need it.

The closest call was deleting `erosion_delta`. The case for keeping it:
accretion may hide sibling branches and so raise PAR risk indirectly, and
root causes name only the proximate mechanism. It was rejected because
`diff_impact`'s threshold on changed symbols delivers most of the value
without 1.2k lines of base/head pairing and a known twin-mispairing limit.

## Tasks

- Deletion (AFK, in order): **473** scoring/trend/runs/snapshot detail →
  **474** biomarkers/findings/waivers, **475** erosion.
- Surviving-signal fixes: **476** behavioral_coupling bulk-commit and mod-edge
  noise, plus its swallowed errors. **477** dead-code resolver correctness
  (prereq for orphans).
- Closed wontfix: 405 (source sets: only test/non-test needed, which already
  exists), 407 (calibration: findings wrong at the source; moved to 476/477),
  452 (erosion gate: no accretion evidence), 453 (saturation constants).
  406 (partial-result discipline) was folded into 469 as an acceptance
  criterion.
