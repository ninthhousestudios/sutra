# Brainstorm: constraint system + DD design

Phase 5d brainstorm. Decisions ready for PRD.
Task: sutra/42. Context: `sutra-vision.md` (L3), `sutra-architecture.md`,
`prd-core-model.md`, DD spike (`docs/v1-spikes/differential-dataflow.md`
on `spike/hdc-ast-encoding`).

## DD scope

Maintained views (DD-powered, automatically incremental):
- **Cycle detection** — transitive closure self-loops, SCC decomposition.
  Already implemented.
- **Blast radius** — transitive reachability count per node. Already
  implemented.
- **Forbidden deps** — edge intersection with forbidden set. Currently
  ad-hoc at query time; promote to maintained view so violations update
  instantly on edge changes.
- **Boundary enforcement** — component-level "X must not import Y" resolved
  to file-level forbidden pairs (see Boundary Enforcement below).
- **Transitive reachability** — the TC is already computed internally;
  expose as a queryable view for impact analysis.

Ad-hoc / imperative (no DD):
- **PageRank** — non-monotone iteration, spike confirmed DD is a bad fit.
- **Co-change** — git-based, no incremental benefit from DD.
- **One-shot parameterized queries** — deps-from-root, single-symbol
  impact. DD overhead not justified.

Component-level aggregations (coupling between components, component health
rollups): deferred. Component membership changes infrequently; the
incrementality benefit is thin. Revisit if real-time component dashboards
become a goal.

## Constraint authoring

Hybrid approach: well-typed structured patterns for common constraint kinds,
with a future Datalog-like escape hatch for custom rules.

Current `rules.toml` expands with new constraint kinds. Each constraint
carries:
- **kind** — which DD view / check it maps to
- **severity** — blocking, advisory, or informational (per-instance, set
  by author)
- **provenance** — optional reference to ADR, design doc, or rationale
- **scope** — optional component or path restriction

### Constraint kinds (initial set)

| Kind | Description | DD view? |
|---|---|---|
| `forbidden_dep` | File/module A must not import B | Yes (maintained) |
| `boundary` | Component X must not import component Y | Yes (resolved to file-level forbidden pairs) |
| `max_fan_in` | File/symbol must not exceed N dependents | Threshold on blast radius view |
| `no_cycles` | Specified subgraph must be acyclic | Filter on cycle view |

Future kinds (post-PRD): required-interface patterns, layering rules
(A may import B but not vice versa), max-coupling between components.

### Format evolution

Current:
```toml
[constraints]
forbidden_deps = [{from = "guard.rs", to = "error.rs"}]
```

Target:
```toml
[[constraint]]
kind = "forbidden_dep"
from = "src/guard.rs"
to = "src/error.rs"
severity = "blocking"
provenance = "docs/adr/0005-guard-isolation.md"

[[constraint]]
kind = "boundary"
from_component = "db"
to_component = "http"
severity = "blocking"

[[constraint]]
kind = "no_cycles"
scope = "src/core/"
severity = "blocking"
```

The `[[constraint]]` array replaces the current `[constraints]` section.
Migration: read old format, write new format on first edit. Both formats
supported during transition.

## Boundary enforcement

Component boundaries as first-class constraints. Resolution strategy:

```
component_membership changes
  → resolve boundary constraints to file-level forbidden pairs
  → feed resolved pairs to DD as a maintained view
  → violations update on: file edits (new edges) AND component
    recomputation (membership changes)
```

DD operates on file IDs only — it doesn't know about components. The
resolution layer sits between the constraint store and DD input, joining
`component_membership` with `boundary` constraints to produce concrete
`(file_id, file_id)` forbidden pairs.

This keeps DD simple and the component abstraction in one place (the
resolver).

## ADRs and constraints

Not a separate system. The workflow:

1. Human and agent make an ADR.
2. In the same session, agent writes a constraint encoding the ADR's
   checkable rule.
3. The constraint's `provenance` field points at the ADR.

This is human-supervised LLM authoring — the agent proposes the constraint,
the human reviews it alongside the ADR. No auto-parsing, no magic
extraction. The constraint format (above) is the interface.

## Real-time checking — latency tiers

| Trigger | What runs | Latency target |
|---|---|---|
| On-save | tree-sitter reparse → delta → DB update → DD ingest → constraint violations | <100ms |
| On-review (`sutra review`) | Full FCA + DD + drift + templates + health | Seconds OK |
| On-hook (guard) | Same as review, blocking pre-commit | <2s |

No on-keystroke checking. The full on-save pipeline is: tree-sitter
reparse (~5ms) + delta extraction + DB update (~10ms) + DD ingest (~60μs)
+ result propagation. Well within 100ms for single-file changes.

## DD engine evolution

### Multiple input collections

Current: one input (import edges). Target: two inputs:
1. **Import edges** — from Layer 0 parse deltas (as today).
2. **Forbidden pairs** — from constraint resolution. Updated when
   constraints change or component membership changes.

Forbidden dep checking becomes a maintained DD view (edge ∩ forbidden →
violation) instead of ad-hoc intersection at query time.

### Richer violation output

Current `ConstraintViolation`: `from_id`, `to_id`, `rule_from`, `rule_to`.
Add: `constraint_kind`, `severity`, `provenance`, `component_context`.
Violations carry enough context for the review report and orient output
without re-joining against the constraint store.

### Memory model

DD state is in-memory only. Sutra's DB is source of truth; DD is a
maintained cache. On restart, DD reloads from DB. At sutra scale (thousands
of files), memory is not a concern. The cold → warm transition (3.9ms
spike measurement) is acceptable.

### Eviction

Current `evict_if_idle` with configurable timeout. Keep as-is. DD worker
thread is cheap when warm; evicting saves memory for idle workspaces.

### No persistence of DD state

DD doesn't serialize its dataflow graph. Rebuilding from DB on restart is
fast enough and avoids serialization complexity.

## Trust model integration

### Severity: per-constraint-instance

Each constraint declaration carries its own severity (blocking, advisory,
informational). The author decides when writing the constraint. Defaults:
- `forbidden_dep`: blocking
- `boundary`: blocking
- `max_fan_in`: advisory
- `no_cycles`: blocking

### Waivers

Same pattern as convention waivers (`convention_waivers` table). New
`constraint_waivers` durable table:
- `constraint_id` (TEXT, matches constraint identity)
- `file_path` or `symbol` (what's waived)
- `rationale` (required)
- `created_at`, `created_by`

Waivers are tracked, not silent — they appear in every review report that
touches the waived area.

### Sketch mode interaction

Per ADR-0001: in sketch mode, conventions flatten to informational but
constraints remain enforced. This is correct — constraints encode
architectural invariants that matter even during prototyping. A spike that
violates a constraint may produce meaningless results.

## Module rename: dd/ → constraints/

First PR of 5d implementation. Mechanical refactor:
- `src/dd/` → `src/constraints/`
- `mod.rs`, `engine.rs`, `worker.rs` — same structure, new name
- Update all `use crate::dd::` imports
- Update `mod dd` in `lib.rs`

Aligns with architecture doc's naming-by-concern convention. Low risk,
sets vocabulary for the rest of 5d.

## Independence from 5c (conventions)

Coupling points and how they stay clean:

| Integration point | Ownership | Discipline |
|---|---|---|
| Review pipeline (`tools/review.rs`) | Shared aggregator | Calls both engines, merges findings. Neither engine knows about the other. |
| Rules file (`rules.toml`) | Shared config | Both sections in one file is fine. Separate parsing, separate types. |
| Guard hook | Shared aggregator | Checks both, reports both. Thin. |
| Orient | Shared presenter | Convention-aware (5c-9). Becomes constraint-aware too — "constraints that apply here." |
| DB | Shared storage | Separate tables, no foreign keys between convention and constraint tables. |
| Trust model | Shared framework | Same severity/waiver vocabulary, separate waiver tables. |

No data dependency: constraints operate on Layer 0 structural edges,
conventions operate on Layer 0 + FCA attributes. They share infrastructure
(DB, review pipeline, orient, guard) but are semantically independent.

## Open questions for PRD

- **Constraint identity**: hash of (kind + params)? Or human-assigned name?
  Needs to be stable for waiver references.
- **Constraint file location**: `.sutra/constraints.toml` vs workspace-root
  `rules.toml` (current)? Or support both (workspace-level + per-directory)?
- **Forbidden pair resolution caching**: when component membership doesn't
  change, skip re-resolution. Cache invalidation strategy.
- **DD worker API**: current command/response channel design works but is
  verbose. Worth cleaning up as part of the rename PR, or leave for later?
- **Constraint diffing in review**: "this PR added/removed these constraint
  violations" requires comparing pre/post state. DD handles this via
  epochs, but the reporting layer needs design.
