# behavioral_coupling noise fixes and additive-PAR back-test (sutra/476)

Review's `behavioral_coupling` (`src/tools/review.rs`) lists co-change partners
of the changed files that have no static edge to them and weren't touched by
the diff. sutra/464 kept it as the one git signal already shaped to a diff, and
as the candidate mechanism for additive PAR, which the sibling-pattern check
misses by construction ([sibling-pattern-backtest.md](sibling-pattern-backtest.md)).
The vidhi/review/5 pilot found the same query 3 of 11 actionable, and wrong on
bulk commits and on Rust parent/child modules.

## Verdict

- **Noise: fixed.** Varuna's list drops from 888 workspace-wide pairs to 0, and
  every other pilot repo drops by 60–75%. The survivors look like real
  parallel siblings.
- **Additive PAR: 0 of 4, and not fixable by tuning.** Three of the four missed
  sites were in a file the first fix already touched, and file-level co-change
  can't point back into the diff. In the fourth, two of the three missed files
  have a static edge to the changed file, and the "no static edge" rule filters
  them by design. The third co-changed in 1 of 6 commits. Additive PAR needs
  symbol-level sibling knowledge (near-duplicate siblings, declared shared
  responsibility), not file co-change.

## What was wrong

1. **The bulk-commit cap counted indexed files only.** `commit_files` holds only
   indexed paths, so the 50-file fan-out cap (sutra/324) saw a 99-path varuna
   "core: sync" as 23 files and a 48-path one as 39. Those two commits alone
   produced ~990 jaccard-1.0 pairs.
2. **Module declarations were not static edges.** `static_file_edges` read only
   resolved `refs`. A parent that just declares `pub mod x;` names no child
   symbol, so the pair counted as "no static edge".
3. **Single-shared-commit pairs.** With jaccard ≥ 0.5 and one shared commit,
   both files were barely touched: born in the same commit (backend
   `envelope/`), or swept by one small sync or a mechanical sweep (a 9-file
   `src/tools/*` commit in sutra gave a 30-pair clique).
4. **Swallowed errors.** A failed config load, co-change query, file listing or
   edge read all returned an empty list, which reads as "no partners".

## Fixes

| Fix | Where | Scope |
|---|---|---|
| `commits.file_count` = every path git reported. The cap applies to it and drops from 50 to 30 (sutra p99 = 32 over 180 days; the ROSE cutoff). NULL rows from before migration 0087 fall back to the indexed count. | `history.rs`, `db/graph.rs` `cochange_pairs_above_threshold`, migration 0087 | Shared: review and component clustering |
| `static_file_edges` = resolved refs ∪ resolved imports, including `kind = 'mod'`. | `db/graph.rs` | Review only (the only caller) |
| A partner needs ≥ 2 shared commits (`MIN_PARTNER_SHARED_COMMITS`). Entity co-change already uses the same floor. | `review.rs` | Review only. Clustering still reads single-commit pairs, where "born together" is weak evidence of the same component. |
| `behavioral_coupling` returns `Result`, and review emits `behavioral_coupling_error`. | `review.rs` | Review |

## Noise measurement

Workspace-wide pairs, replaying the review filter on copies of the live
indexes. This is the superset of partners any single diff could see.
([`experiments/behavioral-coupling/`](../experiments/behavioral-coupling/), `run.sh`.)

| Repo | Commits (>30 paths) | Before | Cap fix | + import edges | + ≥2 shared |
|---|---|---|---|---|---|
| sutra | 337 (5) | 72 | 54 | 54 | 19 |
| backend | 322 (5) | 23 | 22 | 19 | 6 |
| swe_dashboard | 356 (11) | 50 | 48 | 45 | 19 |
| varuna360-core | 25 (6) | 888 | 9 | 7 | 0 |

- **Import edges** removed backend `middleware/mod.rs`↔`security_headers.rs` and
  `chat-store/lib.rs`↔`ids.rs`/`rows.rs`, plus 3 swe_dashboard pairs linked
  only by an import (`atlas_store`↔`atlas_store_io` and others). They removed
  nothing in sutra: none of its co-change pairs was import-only. The pilot's `findings.rs`↔`health/mod.rs` example is gone with
  the health layer, so it couldn't be replayed.
- **Single-shared-commit pairs (81 across the four repos).** About 6 were
  worth a look: platform `io`/`stub` twins in swe_dashboard and
  `c_imports`↔`rust_imports`. The rest were born-together or sweep cliques.
  The twins resurface once they co-change a second time.
- **Survivors at ≥ 2 shared** look like parallel siblings: `parser/c.rs`↔
  `python.rs` (8 shared), `tools/calls.rs`↔`refs.rs`/`impact.rs`,
  swe_dashboard's `*_tab.dart` family (15–18 shared). These are the PAR-shaped
  pairs the mechanism exists for. They are not labelled against later fixes.

## Additive-PAR back-test

Each case replays the partner list at the parent of the **first** fix
(90-day window, new cap). Static edges can't be reconstructed at the parent, so
the list is unfiltered, which only makes it more generous.

| Pair | First fix | Missed site | Named? | Why not |
|---|---|---|---|---|
| sutra/436 → 455 | 6e5d60e | `tools/trend.rs` workspace path | no | Same file as the first fix. |
| sutra/219 → 220 | ac25e6e | `constraints/check.rs` | no | Same file. |
| adityas/ai/110 → 115 | a06a497 | `chat/engine.rs` `ai.tool` log | no | Same file. |
| sutra/292 → 296 | 723ac70 | `constraints/check.rs`, `external.rs`, `mod.rs` | no | From `patterns.rs`: `check.rs` co-changed in 5 of 6 commits, but jaccard is 0.14 because check.rs is a hub, and it has a static edge anyway. `mod.rs` is its module parent (a static edge now). `external.rs` shared 1 of 6. |

Directional confidence (ROSE's P(partner | changed) instead of symmetric
jaccard) would score `patterns.rs → check.rs` at 0.83. It still wouldn't fire,
because the pair is statically linked, and dropping the static-edge rule brings
back every caller as a "partner". Not pursued.

## Not changed

- `cochange_for_file` (the `sutra_cochange` tool) has no fan-out cap at all.
  It's a raw lookup, not an advisory, so it is out of scope here.
- History freshness on the review path (see the note on sutra/476). The bulk
  and min-support filters read ingested history. A commit made since the last
  reparse shows up after the next one, and one commit can't satisfy the
  ≥ 2 floor on its own, so no targeted history refresh was added.

## Caveats

- The measurement approximates `is_test_file` with a regex, and the back-test
  approximates "indexed" by extension plus existence at the parent.
- Survivors are labelled by path shape only, not by later fix commits.
- n = 4 for additive PAR. The same-file finding is structural, though: it holds
  for any file-granular co-change signal.
