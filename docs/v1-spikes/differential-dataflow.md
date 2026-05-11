# Spike: differential dataflow for structured reasoning

Task: sutra/v1/2

## Question

Can differential dataflow replace Sutra's ad-hoc graph analyses with a
unified, automatically incremental, temporally aware reasoning engine?

## Verdict: viable with caveats

DD is a strong fit for Sutra's graph analyses (deps, impact, co-change,
constraints). The automatic incrementality is real and impressive. But it
introduces significant complexity and has hard limits around iterative
convergence algorithms (pagerank).

## Results

### Analyses ported (3/3)

| Analysis | DD result | Ref result | Match |
|----------|-----------|------------|-------|
| deps (BFS reachability) | 1 node, 1ms | 1 node, 77us | exact |
| impact (transitive closure) | 11 files, 681us | 5 files, 10.6ms | superset (DD=full TC, ref=depth-3) |
| co-change (self-join) | 69 files, 310us | 69 files, 42us | exact (69/69 counts match) |

Impact shows more files because DD does full transitive closure at file level
while the reference does depth-limited symbol-level BFS. DD's answer is
arguably more complete.

### Incremental update latency

Test: 72 files, 292 edges (sutra workspace, ~971 symbols).

| View | Load | No-op reparse | 1-edge change |
|------|------|---------------|---------------|
| Simple (fan-in + out-degree) | 84us | 38us | **19us** |
| Transitive reachability | 3.9ms | — | **59us** |

Both well under the 100ms target. The transitive closure loads in 3.9ms but
incrementally updates in 59us — that's the DD value proposition.

### Architectural constraints (2 maintained views)

1. **Cycle detection** via transitive closure self-join: found 49 files in
   dependency cycles. Maintained live — adding/removing an edge instantly
   updates the violation set.

2. **Forbidden dependency enforcement**: edge intersection with a forbidden
   set. Adding `(guard.rs, error.rs)` as forbidden correctly flagged the
   existing edge.

### Temporal queries

Demonstrated multi-epoch state: fan-in of `error.rs` = 39 at epoch 0,
decreases to 38 at epoch 1 after removing one incoming edge. Each epoch
is a queryable snapshot.

### PageRank

DD's `iterate` is designed for monotone fixed-points (reachability, shortest
paths). PageRank's power iteration redistributes mass each round — not
monotone. An epoch-loop (external iteration) works but loses automatic
incrementality. This is a fundamental mismatch.

## LOC comparison

| | Current (ad-hoc) | DD spike |
|--|--|--|
| deps | 88 LOC | ~30 LOC (dataflow) |
| impact | 108 LOC | ~20 LOC (dataflow) |
| co-change | 46 + 99 LOC (git) | ~20 LOC (dataflow) + git loader |
| graph.rs (rollups, pagerank) | 259 LOC | N/A (maintained views) |
| **Total analysis code** | ~600 LOC | ~100 LOC dataflow + ~80 LOC harness |

DD dataflow definitions are more concise. But the harness code (input sessions,
probing, result extraction) adds overhead, especially for reading results.

## Ergonomics assessment

**Strengths:**
- Concise dataflow definitions for graph queries
- Automatic incrementality is transformative for maintained views
- Temporal queries via epochs are natural
- `iterate` handles BFS/reachability elegantly

**Weaknesses:**
- Learning curve is steep — DD's type system is complex, error messages opaque
- Rust 2024 edition + DD's ownership model requires liberal `.clone()` on collections
- Result extraction requires Arc/Mutex plumbing (timely requires Send+Sync)
- Iterative convergence (pagerank) doesn't fit the monotone lattice model
- Debugging dataflow computations is harder than stepping through imperative BFS
- Dependency weight: timely (0.29) + differential-dataflow (0.23) + ~15 transitive deps

**API friction in DD 0.23:**
- `iterate` takes `(scope, variable)` — scope arg is needed for `enter()`
- All collection methods take `self` by value — must clone before branching
- `distinct_total()` only works outside iterate (needs TotalOrder); use `distinct()` inside
- `join_map` / `semijoin` take collections by value, not reference
- No way to "peek" at current state without trace cursors (complex) or inspect (delta-based)

## Recommendation

**Use DD for maintained views** (cycle detection, forbidden deps, fan-in/blast-radius
rollups). These are the "killer app" — they stay live as the codebase evolves and
DD handles incrementality automatically.

**Keep imperative code for one-shot queries** (deps from a root, impact of a symbol).
The DD overhead isn't worth it for parameterized, on-demand queries that don't benefit
from maintained state.

**Don't use DD for pagerank.** Keep the current power-iteration implementation.

**Consider for co-change** if you want incremental updates as new commits arrive.
The DD self-join is elegant but the current git-log approach is simple enough.
