# Constraint system architecture map

Quick-reference for agents planning or implementing constraint-system tasks.
Read this first, then do targeted `sutra_outline` / `sutra_read` calls on
specific files. Updated after each constraint-system landing.

Last updated: 2026-06-04 (5d-8: guard severity filtering)

## Module layout

```
src/constraints/
  mod.rs            — re-exports DdEngine, ConstraintResolver; public types:
                      Cycle, DdFacts, DdDelta, ConstraintViolation (legacy).
                      Shared helpers: find_matching_constraint,
                      build_component_context, format_violation_detail.
  engine.rs         — DdEngine (Cold/Loaded/Warm state machine), public API:
                      ingest, update, set_forbidden_pairs, query_violations,
                      query_cycles, query_blast_radius[_all], evict_if_idle.
                      query_forbidden_deps (deprecated, no callers).
  resolver.rs       — ConstraintResolver: resolves Constraint rules to
                      forbidden (i64, i64) pairs. Handles ForbiddenDep (glob)
                      + Boundary (component membership). Caches by input hash
                      + clustering generation.
  worker.rs         — timely/DD worker thread, Command/Response enums,
                      WorkerHandle, spawn_worker, run_worker (dataflow +
                      command loop), Kosaraju SCC

src/rules.rs        — TOML parsing for .sutra/rules.toml.
                      Types: Severity, ConstraintKind, Constraint, RawConstraint,
                      Rules, Constraints, ForbiddenDep, ConventionsConfig.
                      Functions: parse_rules, load_rules, Rules::all_constraints.

src/db/
  constraints.rs    — ConstraintWaiverRow, CRUD for constraint_waivers table.
                      get_constraint_waivers, get_constraint_waivers_for_file,
                      create/update/delete, reconcile_orphaned_constraint_waivers.

src/tools/
  review.rs         — build_findings uses ConstraintResolver +
                      set_forbidden_pairs + query_violations maintained view.
                      Enriched ConstraintViolation with constraint metadata.
                      Constraint waiver partition + DdDelta violation diffing.
  orient.rs         — handle() includes constraints section per component.
                      constraints_for_component: scope matching (path prefix,
                      glob, boundary, max_fan_in, no_cycles).
                      compute_violations: DD engine ingestion + violation query.
                      Filters violations + constraint waivers to component files.

src/guard.rs        — Lightweight per-edit constraint check.
                      check_file_constraints: queries imports table + rules TOML
                      directly from read-only SQLite connection. Matches edges
                      against ForbiddenDep/Boundary constraints, checks waivers.
                      ConstraintFinding type with severity + waived flag.
                      format_constraint_deny for deny-reason formatting.

src/bin/guard.rs    — Guard binary (Claude Code PreToolUse hook).
                      PreToolUse path: lightweight check, blocking → deny,
                      advisory/informational → stderr, waived → silent.
                      --check-constraints mode: full build_findings with
                      ephemeral DdEngine, structured JSON output, exit code 1
                      if blocking violations exist. Supports --staged flag.
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

### ConstraintResolver (resolver.rs)
Resolves `Vec<Constraint>` to `Vec<(i64, i64)>` forbidden pairs. For
ForbiddenDep: glob-matches paths in the path_map. For Boundary: looks up
component membership via DB. Caches result keyed by `(input_hash,
clustering_generation)` — invalidate on component recompute. Used by
build_findings before calling `set_forbidden_pairs`.

### ConstraintWaiverRow (db/constraints.rs)
`{ id, constraint_id, constraint_name, file_path, symbol_qualified_name,
rationale, waived_by, created_at, updated_at }`. Waiver lookup in review:
match on `constraint_id` + `file_path` (either from_path or to_path).

### DdFacts / DdDelta (mod.rs)
`DdFacts { import_edges: Vec<(i64, i64)> }` — initial edge set.
`DdDelta { added_edges, removed_edges }` — incremental update.

### ConstraintViolation (review.rs)
Enriched review-level type: `{ constraint_id, constraint_name, constraint_kind,
severity, provenance, from_path, to_path, component_context, detail }`.
Built by matching maintained view violations `Vec<(i64, i64)>` back to
constraints via glob/component re-check. Detail string tagged `[introduced]`
for violations caused by changed files' imports (DdDelta round-trip).

### WaivedConstraintViolation (review.rs)
Same fields as ConstraintViolation plus `rationale` and `waived_by`. Partitioned
from violations using constraint_waivers DB table (parallel to convention waivers).

### ConstraintFinding (guard.rs)
Lightweight per-edit type: `{ constraint_id, name, kind, severity: Severity,
from_path, to_path, detail, waived: bool }`. Produced by `check_file_constraints`
which queries the imports table directly (no DD engine). Used in PreToolUse hook
to block on Blocking severity.

### ConstraintViolation (mod.rs, legacy)
Legacy type from deprecated ad-hoc path: `{ from_id, to_id, rule_from, rule_to }`.
Only used by deprecated `query_forbidden_deps`. No callers in current code.

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
| sutra/69 | rename dd/ → constraints/ | done | — | src/constraints/, lib.rs, config.rs |
| sutra/70 | constraint types + rules parsing | done | 69 | src/rules.rs |
| sutra/71 | DD forbidden pairs maintained view | done | 69 | src/constraints/{worker,engine}.rs |
| sutra/72 | boundary resolver | done | 70, 71 | src/constraints/resolver.rs |
| sutra/73 | constraint waivers (DB) | done | 70 | src/db/constraints.rs, migrations |
| sutra/74 | review integration | needs-review | 71, 73 | src/tools/review.rs |
| sutra/75 | orient constraint section | done | 70, 73 | src/tools/orient.rs |
| sutra/76 | guard severity filtering | done | 74 | src/guard.rs, src/bin/guard.rs |
| sutra/77 | MCP constraint tools | ready-for-agent | 71, 73 | src/tools/constraints.rs (new) |
| sutra/78 | review-1: foundation | done | 69-71 | — |
| sutra/79 | review-2: resolver + waivers | ready-for-human | 72, 73 | — |
| sutra/80 | review-3: integration | ready-for-human | 74-77 | — |

## Design docs

- PRD: yojana task `sutra/68` (full constraint system design)
- Brainstorm: yojana task `sutra/42`
- DD spike: `docs/v1-spikes/differential-dataflow.md` (experiment 4)

## Test locations

- Unit tests: `#[cfg(test)]` in `src/rules.rs` (22 tests — parsing, identity, defaults, errors)
- Integration tests: `tests/constraints-test.rs` (27 tests — cycles, blast radius,
  forbidden deps ad-hoc, maintained violations, eviction/rewarm)
- Review integration: `tests/review-test.rs` (22 tests — maintained view, waiver partition,
  delta labels, enriched violation fields, compute serialization)
- Orient constraints: `#[cfg(test)]` in `src/tools/orient.rs` (8 constraint tests — scope
  matching by prefix/boundary/glob, out-of-scope exclusion, waivers, violations, sketch mode)
- Guard constraint filtering: `#[cfg(test)]` in `src/guard.rs` (9 new tests — severity
  filtering, waiver bypass, lightweight check with in-memory SQLite, advisory passthrough)
- Test engine setup: `DdEngine::new(Duration::from_secs(1800))`, no DB needed
- Test DB setup (waivers): `Db::open_unchecked("test", dir.path())` with tempdir
