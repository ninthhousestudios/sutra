# Constraint system architecture map

Quick-reference for agents planning or implementing constraint-system tasks.
Read this first, then do targeted `sutra_outline` / `sutra_read` calls on
specific files. Updated after each constraint-system landing.

Last updated: 2026-06-03 (5d-3: DD forbidden pairs maintained view)

## Module layout

```
src/constraints/
  mod.rs            — re-exports DdEngine; public types: Cycle, DdFacts,
                      DdDelta, ConstraintViolation
  engine.rs         — DdEngine (Cold/Loaded/Warm state machine), public API:
                      ingest, update, set_forbidden_pairs, query_violations,
                      query_cycles, query_blast_radius[_all], evict_if_idle.
                      query_forbidden_deps (deprecated, ad-hoc glob path).
  worker.rs         — timely/DD worker thread, Command/Response enums,
                      WorkerHandle, spawn_worker, run_worker (dataflow +
                      command loop), Kosaraju SCC

src/rules.rs        — TOML parsing for .sutra/rules.toml.
                      Types: Severity, ConstraintKind, Constraint, RawConstraint,
                      Rules, Constraints, ForbiddenDep, ConventionsConfig.
                      Functions: parse_rules, load_rules, Rules::all_constraints.

src/tools/
  review.rs         — build_findings calls query_forbidden_deps (deprecated,
                      migration to query_violations is sutra/74)
```

## Key types

### Constraint (rules.rs)
Authored rule from `.sutra/rules.toml`. Fields: `id` (blake3 hash, 8 hex chars),
`kind: ConstraintKind`, `severity: Severity`, `name: Option<String>`,
`provenance: Option<String>`, `scope: Option<String>`.

### ConstraintKind (rules.rs)
Enum: `ForbiddenDep { from, to }` (glob patterns), `Boundary { from_component,
to_component }`, `MaxFanIn { target, threshold }`, `NoCycles`.

### Severity (rules.rs)
Enum: `Blocking`, `Advisory`, `Informational`.
Defaults: forbidden_dep/boundary/no_cycles → Blocking, max_fan_in → Advisory.

### Constraint identity (rules.rs)
blake3 hash of `(kind_tag, kind-specific params, scope)`. Name and provenance
are excluded — name is an alias for human reference, not identity. Truncated
to 8 hex chars, matching convention ID style.

### DdEngine (engine.rs)
State machine: `Cold` → `Loaded { edges, forbidden_pairs }` → `Warm { handle,
edges, forbidden_pairs, last_query }`. Transitions:
- `ingest()`: Cold → Loaded (once only)
- `ensure_warm()`: Loaded → Warm (spawns worker, sends edges + forbidden pairs)
- `evict_if_idle()`: Warm → Loaded (preserves edges + forbidden pairs)
- Drop: → Cold (shuts down worker)

### DdFacts / DdDelta (mod.rs)
`DdFacts { import_edges: Vec<(i64, i64)> }` — initial edge set.
`DdDelta { added_edges, removed_edges }` — incremental update.

### ConstraintViolation (mod.rs)
Legacy type from ad-hoc path: `{ from_id, to_id, rule_from, rule_to }`.
The maintained view returns raw `Vec<(i64, i64)>` instead — richer violation
output with constraint metadata is sutra/74's scope.

## DD worker internals (worker.rs)

Two input collections sharing one timestamp + probe:
1. **edges** `InputSession<(i64, i64)>` — import graph
2. **forbidden** `InputSession<(i64, i64)>` — pre-resolved forbidden pairs

Three maintained views (all probed):
- **Transitive closure** → cycle nodes (self-loops in TC, via `iterate`)
  → Kosaraju SCC on query
- **Blast radius** → `count_total` of transitive reachability per node
- **Violations** → `edges.semijoin(forbidden)` — intersection of direct
  edges with forbidden pairs

Command/Response protocol (crossbeam channels, blocking recv):
- `Ingest(edges)` → `Ok` — initial load, advances both inputs
- `Update { added, removed }` → `Ok` — incremental edge change
- `SetForbiddenPairs(pairs)` → `Ok` — full replacement, diffs against
  stored set, advances both inputs
- `QueryCycles` → `Cycles(Vec<HashSet<i64>>)`
- `QueryBlastRadius(node)` → `BlastRadius(usize)`
- `QueryBlastRadiusAll` → `BlastRadiusAll(HashMap<i64, usize>)`
- `QueryViolations` → `Violations(Vec<(i64, i64)>)` — sorted
- `Shutdown` — break loop, joined on WorkerHandle drop

Critical invariant: every mutation handler advances BOTH inputs to the same
timestamp and flushes both before stepping. Missing this stalls the probe.

## TOML format (.sutra/rules.toml)

### New format (canonical)
```toml
[[constraint]]
kind = "forbidden_dep"       # required
from = "src/tools/*"         # kind-specific
to = "src/daemon.rs"         # kind-specific
severity = "blocking"        # optional, defaults per kind
name = "no-tool-daemon"      # optional, human label
provenance = "docs/adr-001"  # optional, rationale/ADR
scope = "src/"               # optional, path prefix

[[constraint]]
kind = "boundary"
from_component = "db"
to_component = "http"

[[constraint]]
kind = "max_fan_in"
target = "src/config.rs"
threshold = 10

[[constraint]]
kind = "no_cycles"
scope = "src/core/"
```

### Old format (backward compat)
```toml
[constraints]
forbidden_deps = [
  { from = "src/tools/*", to = "src/daemon.rs" },
]
```

Parsed via `Rules::all_constraints()` which merges both formats, converts
old `ForbiddenDep` entries to `Constraint` with kind=forbidden_dep,
severity=blocking. Deduplicates by constraint ID (first-seen wins).

## Remaining tasks (5d arc)

| Task | Title | Status | Depends on | Key files |
|---|---|---|---|---|
| sutra/69 | rename dd/ → constraints/ | needs-review | — | src/constraints/, lib.rs, config.rs |
| sutra/70 | constraint types + rules parsing | needs-review | 69 | src/rules.rs |
| sutra/71 | DD forbidden pairs maintained view | needs-review | 69 | src/constraints/{worker,engine}.rs |
| sutra/72 | boundary resolver | ready | 70, 71 | src/constraints/resolver.rs (new) |
| sutra/73 | constraint waivers (DB) | ready | 70 | src/db/constraints.rs (new), migrations |
| sutra/74 | review integration | ready | 71, 73 | src/tools/review.rs |
| sutra/75 | orient constraint section | ready | 70, 73 | src/tools/orient.rs |
| sutra/76 | guard severity filtering | ready | 74 | src/bin/guard.rs |
| sutra/77 | MCP constraint tools | ready | 71, 73 | src/tools/constraints.rs (new) |
| sutra/78-80 | review gates (HITL) | ready-for-human | various | — |

## Design docs

- PRD: yojana task `sutra/68` (full constraint system design)
- Brainstorm: yojana task `sutra/42`
- DD spike: `docs/v1-spikes/differential-dataflow.md` (experiment 4)

## Test locations

- Unit tests: `#[cfg(test)]` in `src/rules.rs` (22 tests — parsing, identity, defaults, errors)
- Integration tests: `tests/constraints-test.rs` (27 tests — cycles, blast radius,
  forbidden deps ad-hoc, maintained violations, eviction/rewarm)
- Review integration: `tests/review-test.rs` (uses deprecated `query_forbidden_deps`)
- Test engine setup: `DdEngine::new(Duration::from_secs(1800))`, no DB needed
- Test DB setup (for future waivers): `Db::open_unchecked("test", dir.path())` with tempdir
