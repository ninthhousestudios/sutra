# Spike: Salsa incremental computation (boundary-finding)

Task: sutra/v1/4

## Question

Does Salsa fill a distinct role (fine-grained per-file query memoization) or
is it redundant with differential dataflow + SQLite?

## Verdict: skip

Salsa's per-query memoization is impressive within a file but doesn't solve
sutra's actual bottleneck (cross-file invalidation). The current SQLite +
content-hash approach already achieves file-level skip, and Salsa can't beat
that for cross-file aggregation. Adding it would mean a third computation
framework alongside DD and SQLite, for marginal gain.

## Results

### Single-file pipeline (Salsa's sweet spot)

| Operation | Cold | Warm (memoized) | Speedup |
|-----------|------|-----------------|---------|
| Parse largest file (47KB) | 66ms | 7us | 9,600x |
| Outline (depends on parse) | 73us | 4us | 18x |
| Reparse after trivial edit | 66ms | — | (must reparse) |
| Outline after trivial edit | 61us | — | (recomputed) |

The 9,600x warm speedup is real — Salsa returns the memoized ParsedFile
without touching tree-sitter. But the reparse-after-edit (66ms) is identical
to cold parse because any text change invalidates the parse query entirely.
Salsa can't do partial re-parse (that's a tree-sitter problem).

The outline-after-edit (61us) demonstrates Salsa's value: it re-derives
the outline from the new ParsedFile rather than returning the stale cache,
and the cost is just Vec construction.

### Bulk parse (session-level)

| Operation | Time |
|-----------|------|
| Cold parse 74 files | 983ms |
| Warm read 74 files | 57us |
| Re-read all after 1 file changed | 8.2ms |

The 1-file-changed case is key: 8.2ms vs 983ms cold. Salsa correctly
identifies that only 1 of 74 files needs reparsing. But sutra's current
approach (content hash → skip unchanged) achieves the same result. The
question is whether Salsa's *automatic* detection is worth the dependency
vs sutra's *manual* hash check.

### Cross-file resolution (Salsa's weak spot)

| Operation | Cold | Warm | After 1 file changed |
|-----------|------|------|---------------------|
| Symbol index (aggregates all files) | 11ms | 4us | 11ms (full recompute) |
| Resolve refs (all files) | 18ms | 102us | 17ms (full recompute) |

**This is the finding that decides it.** The symbol index aggregates symbols
from ALL files into one lookup structure. When any single file changes, Salsa
must recompute the entire index because it can't know which portion of the
aggregate was affected. This means cross-file queries (ref resolution, impact
analysis, dependency graphs) gain nothing from Salsa's incrementality.

DD handles this case better: it can incrementally update a maintained view
when one input tuple changes, without recomputing the full join/aggregation.

### Memory

| Component | Size |
|-----------|------|
| Source text | 731 KB |
| Symbols (~1042) | ~203 KB |
| Refs (~33K) | ~3,251 KB |
| **Total estimated** | **~4.2 MB** |

Fine for sutra's scale. Would be ~40-80 MB for a 1000-file codebase —
still acceptable, but SQLite's on-disk model has no memory pressure at all.

### Import edges (DD handoff)

575 import edges extracted from 74 files. Salsa computes per-file edge
lists; DD could consume the union. The handoff is clean conceptually, but
adding Salsa just for this intermediate step is over-engineering — the
current pipeline already produces the same edges.

## Comparison matrix

| Capability | SQLite (current) | Salsa | DD |
|---|---|---|---|
| Per-file parse memoization | hash-based skip | auto dependency | N/A |
| Cross-file invalidation | manual | automatic* | automatic |
| On-demand queries | SQL | tracked fns | probe (awkward) |
| Maintained graph views | recompute | recompute | automatic |
| Memory model | disk | all in memory | all in memory |
| Persistence | yes | no | no |
| Incremental granularity | file-level | query-level | tuple-level |
| Learning curve | low | medium | high |
| Dependency weight | 0 (bundled) | ~19 crates | ~15 crates |

\* Salsa's cross-file invalidation is automatic but *coarse* — aggregation
queries that read all files recompute fully when any file changes.

## Why skip (not complement)

The "complement" architecture would be: Salsa for parse→symbols→outline,
DD for graph analytics, SQLite for persistence. This sounds clean but:

1. **Sutra already has file-level skip.** Content hashing achieves the same
   result as Salsa's input tracking for the parse pipeline. Salsa's advantage
   (query-level granularity) doesn't help because the bottleneck is *parsing*,
   which is all-or-nothing per file.

2. **Cross-file queries are the real cost.** Ref resolution, impact analysis,
   dependency graphs — these are what agents actually call. Salsa doesn't help
   here (aggregation recomputes fully). DD does.

3. **Three frameworks is too many.** Salsa (19 crates) + DD (15 crates) +
   SQLite adds complexity, binary size, and conceptual overhead for anyone
   working on sutra. Two is already pushing it.

4. **No persistence.** Salsa is session-only. Sutra needs cross-restart
   state (the daemon restarts, workspace re-registers). SQLite already
   provides this; adding Salsa means maintaining two caches.

5. **The 9,600x warm speedup is misleading.** It measures "don't do work
   you already did" — any memoization achieves this. The relevant question
   is "how much work can you skip after a change?" and Salsa's answer for
   cross-file queries is "none."

## What Salsa IS good for (but sutra doesn't need)

- **IDE-style queries:** "give me the outline of the file the user is
  editing" — where the same query runs thousands of times per session
  and inputs change keystroke-by-keystroke. rust-analyzer's use case.
- **Deep query DAGs:** when you have 5+ levels of derived computation
  and changes propagate sparsely. Sutra's DAG is shallow (parse →
  symbols → refs → index).
- **Cancellation:** Salsa supports cancelling in-progress queries when
  inputs change. Useful for IDE responsiveness, not for batch indexing.

## Recommendation

**Skip Salsa. Keep SQLite + content hashing for the parse pipeline.
Invest in DD for cross-file incrementality.**

If sutra ever needs IDE-style responsiveness (sub-keystroke latency for
outline queries during editing), revisit Salsa. But that's a different
product than a batch code intelligence server.

## LOC

| Component | Lines |
|---|---|
| Salsa definitions (inputs, tracked structs, tracked fns) | 120 |
| File loading | 50 |
| Experiments | 230 |
| Main | 20 |
| **Total** | **~420** |
