# sutra v1 — spike overview

## mission

A system that builds and maintains a continuously improving,
evidence-backed understanding of a software project, so agents can
debug, plan, modify, and architect with context comparable to a
long-tenured engineer.

## candidate foundations

| Foundation | Role | Spike |
|---|---|---|
| HRR (holographic reduced representations) | Associative memory, concept composition, contributed knowledge | [hrr-semantic](hrr-semantic.md) |
| Differential dataflow | Structured reasoning, incremental computation, temporal queries | [differential-dataflow](differential-dataflow.md) |
| FCA (formal concept analysis) | Convention detection, pattern inference, concept hierarchies | [fca](fca.md) |
| Salsa | Fine-grained computation caching, query memoization | [salsa](salsa.md) |

## sequence

Phase 1 — individual spikes (parallel). Phase 2 — composition
prototype. Phase 3 — architecture doc. Phase 4 — PRD.

HRR and differential dataflow are highest priority (the two pillars).
FCA and Salsa can lag by a week.

## prior art

HRR structural encoding spike exists on current branch (`src/bin/hdc-eval.rs`).
Eval results show HRR viable for structural search (4.4x lift),
decomposition (30%), transform search (57%), and cross-file diff (58%
interpretable). MAP competitive on large C codebases for decomposition
and transforms. Hybrid likely.
