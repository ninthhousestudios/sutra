# Constraint system architecture map

Quick-reference for agents planning or implementing constraint-system tasks.
Read this first, then do targeted `sutra_outline` / `sutra_read` calls on
specific files. Updated after each constraint-system landing.

Last updated: 2026-07-10 (sutra/245-248: constraint ratchet — monotonic severity floor + guard enforcement)

## Module layout

```
src/constraints/
  mod.rs            — re-exports DdEngine, ConstraintResolver; public types:
                      Cycle, DdFacts, DdDelta, ConstraintViolation (legacy),
                      ConstraintCoverage.
                      Shared helpers: find_matching_constraint,
                      build_component_context, format_violation_detail,
                      constraint_coverage (per-field glob/component match counts
                      for dead constraint detection; takes pattern_only_paths as
                      a separate arg — unindexed stubs count for
                      forbidden_pattern only, never for dep-kind globs).
  engine.rs         — DdEngine (Cold/Loaded/Warm state machine), public API:
                      ingest, update, set_forbidden_pairs, query_violations,
                      query_cycles, query_blast_radius[_all], evict_if_idle.
                      query_forbidden_deps (deprecated, no callers).
  resolver.rs       — ConstraintResolver: resolves Constraint rules to
                      forbidden (i64, i64) pairs. Handles ForbiddenDep (glob)
                      + Boundary (component membership). Caches by input hash
                      + clustering generation.
  finding.rs        — ConstraintFinding (shared finding type), FindingDelta enum
                      (Unknown, PreExisting, Introduced, Resolved). Optional
                      location fields: line, snippet, enclosing_symbol (populated
                      for forbidden_pattern, None for dep-kind constraints).
  patterns.rs       — check_forbidden_patterns: per-file tree-sitter pattern
                      matching. Given compiled forbidden_pattern constraints and
                      source files, runs queries and produces findings with
                      location + enclosing symbol resolution. No DD involvement
                      (per-file local pass, precedent: external.rs).
                      File eligibility uses LanguageAdapter::pattern_extensions()
                      (superset of extensions()), so unindexed stub files match.
                      scan_pattern_only_files walks the workspace for extensions
                      that are pattern-eligible but never indexed (.pyi today) —
                      they have no file row, so check.rs finds them on disk
                      (gated on has_patterns; the walk is O(repo files)).
                      is_pattern_only_path classifies a single path, used by
                      review.rs to keep changed stubs alive across the
                      path→file_id reduction. Stub scan sources by scope:
                      Workspace → every stub on disk, ChangedFiles →
                      EvalScope's changed_pattern_only_paths, SingleFile/Edges →
                      none (guard covers those via check_proposed_patterns).
  check.rs          — Unified constraint evaluation. evaluate() dispatches to
                      evaluate_dd (DD-backed: review, orient, sutra_constraints
                      violations) or evaluate_raw (raw SQLite: guard hook).
                      CheckOutcome, EvalScope, FactsSource.
                      Covers: forbidden_dep/boundary via DD maintained view,
                      no_cycles via SCC, max_fan_in via fan_in_files rollup,
                      external via external::check_*, forbidden_pattern via
                      patterns::check_forbidden_patterns, dead_constraint via
                      constraint_coverage. Pattern scan runs before edge-empty
                      early return (patterns are per-file, not edge-based).
                      Waiver partition at the end.
  external.rs       — External-crate constraint checks (forbidden_external,
                      confined_external). Two signals: import (use-statement
                      paths via external_crate_of_import) and manifest (Cargo.toml
                      via cargo_manifest_deps, pubspec.yaml via pubspec_deps).
                      workspace_dep_renames resolves workspace=true aliases.
                      scan_project_files walks for Cargo.toml + pubspec.yaml.
                      check_workspace_externals is the index-side entry point.
  worker.rs         — timely/DD worker thread, Command/Response enums,
                      WorkerHandle, spawn_worker, run_worker (dataflow +
                      command loop), Kosaraju SCC

src/rules.rs        — TOML parsing for .sutra/rules.toml.
                      Types: Severity, ConstraintKind, Constraint, RawConstraint,
                      Rules, Constraints, ForbiddenDep, ConventionsConfig.
                      Functions: parse_rules, load_rules, Rules::all_constraints,
                      scope_matches_path (hybrid glob-or-prefix scope matching,
                      used by match_no_cycles_constraint, constraint_coverage,
                      orient's generic scope filter), match_no_cycles_constraint.

src/db/
  constraints.rs    — ConstraintWaiverRow, CRUD for constraint_waivers table.
                      get_constraint_waivers, get_constraint_waivers_for_file,
                      create/update/delete, reconcile_orphaned_constraint_waivers.
                      ConstraintRatchetRow, ratchet registry:
                      upsert_constraint_ratchet (monotonic floor — never lowers,
                      clears released_at on re-registration),
                      get_constraint_ratchet, get_active_constraint_ratchets,
                      get_all_constraint_ratchets, release_constraint_ratchet.
                      Helper: severity_ordinal (Severity → u8 for floor comparison),
                      active_ratchets_from_conn (shared raw-conn accessor used by
                      both check.rs evaluate paths).

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
  constraints.rs    — MCP tool: sutra_constraints. Actions: list (all
                      constraints with metadata, waiver counts,
                      matched_file_count per field, dead-constraint warning),
                      violations (DD maintained view — forbidden_dep, boundary,
                      no_cycles, max_fan_in, forbidden_external, confined_external,
                      plus dead_constraint informational findings),
                      waive (create constraint waiver),
                      unwaive (revoke waiver). Thin translation over library
                      API + DB CRUD, following conventions.rs pattern.

src/guard.rs        — Lightweight per-edit constraint check.
                      check_file_constraints: queries imports table + rules TOML
                      directly from read-only SQLite connection. Matches edges
                      against ForbiddenDep/Boundary constraints, checks waivers.
                      check_proposed_patterns: introduced-only forbidden_pattern
                      enforcement — parses proposed + disk, multiset-diffs matches
                      by (constraint_id, enclosing_symbol, snippet), denies only
                      when count increased. format_constraint_deny for dep-kind
                      deny messages. format_pattern_deny for pattern deny messages
                      with justification-gate guidance (waive-vs-restructure).
                      Ratchet guard: check_proposed_rules_ratchet — compares
                      proposed rules.toml against the ratchet registry (active
                      ratchets only, released_at IS NULL). Detects deletion
                      and severity-lowering. format_ratchet_deny teaches the
                      release ceremony and strengthen-by-release-then-re-add path.
                      RatchetViolation, RatchetViolationKind types.

src/bin/guard.rs    — Guard binary (Claude Code PreToolUse hook).
                      PreToolUse path: ratchet check runs first for rules.toml
                      edits (not an indexed file, runs before file_row bail).
                      Then pattern check (introduced-only, doesn't need file_id),
                      then dep-kind check. Blocking → deny,
                      advisory/informational → stderr, waived → silent.
                      Pattern findings from dep-kind fallback path filtered out
                      (handled separately with introduced-only semantics).
                      --check-constraints mode: full build_findings with
                      ephemeral DdEngine, structured JSON output, exit code 1
                      if blocking violations exist. Supports --staged flag.
```

## Key types

### Constraint (rules.rs)
Authored rule from `.sutra/rules.toml`. Fields: `id` (blake3 hash, 8 hex chars),
`kind: ConstraintKind`, `severity: Severity`, `name: Option<String>`,
`provenance: Option<String>`, `scope: Option<String>`, `ratchet: bool`.

### ConstraintKind (rules.rs)
Enum: `ForbiddenDep { from, to }` (glob patterns), `Boundary { from_component,
to_component }`, `MaxFanIn { target, threshold }`, `NoCycles`,
`ForbiddenExternal { from, crates, include_dev }`,
`ConfinedExternal { crates, allowed_in, include_dev }`,
`ForbiddenPattern { language, query }` (tree-sitter S-expression).

### Severity (rules.rs)
Enum: `Blocking`, `Advisory`, `Informational`.
Defaults: forbidden_dep/boundary/no_cycles/forbidden_external/confined_external → Blocking,
max_fan_in/forbidden_pattern → Advisory (heuristic rules).

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

### ConstraintRatchetRow (db/constraints.rs)
`{ id, constraint_id, name, rendered_description, severity_floor,
registered_at, released_at, released_by, release_rationale }`.
Ratchet semantics:
- **Registration**: at index time when `ratchet = true` in rules.toml.
  Upsert monotonically raises severity_floor (never lowers).
  Re-registration after release clears released_at (reactivates).
- **Non-waivability**: ratchet_violation findings are appended to active
  list AFTER waiver partition — structurally bypass waivers.
- **Guard enforcement**: check_proposed_rules_ratchet blocks rules.toml
  edits that delete or weaken ratcheted constraints.
- **Drift detection**: check_ratchet_violations in check::evaluate catches
  constraints removed from rules.toml or downgraded below floor at analysis time.
- **Release**: CLI-only ceremony (`sutra ratchet release <id> --rationale`).
  Sets released_at + released_by + release_rationale. Released ratchets are
  excluded from guard and drift checks (WHERE released_at IS NULL).

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

### ConstraintFinding (finding.rs)
Shared finding type used across all evaluation paths: `{ constraint_id,
constraint_name, constraint_kind, severity, provenance, from_path, to_path,
component_context, detail, delta: FindingDelta, line?, snippet?,
enclosing_symbol? }`. Location fields populated for forbidden_pattern findings,
None for dep-kind. Produced by check::evaluate (both DD and raw paths),
patterns::check_forbidden_patterns, and guard::check_proposed_patterns.
FindingDelta: Unknown (pattern/raw), PreExisting/Introduced/Resolved (review
delta labelling).

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
scope = "src/"               # optional; directory prefix OR glob ("src/**") —
                             # literal boundary-prefix match tried first (so
                             # real dirs like src/app/[slug]/ work), glob
                             # fallback when metacharacters present

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

[[constraint]]
kind = "forbidden_pattern"
language = "rust"                # required, selects grammar
query = '(call_expression ...)'  # required, tree-sitter S-expression
name = "no-clone-driven-dev"
severity = "blocking"
scope = "src/"                   # optional, glob-or-prefix (scope_matches_path)
provenance = "CLAUDE.md"
ratchet = true                   # optional, registers in ratchet registry at
                                 # index time. Floor never lowers; removal or
                                 # weakening requires `sutra ratchet release`.
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
| sutra/77 | MCP constraint tools | needs-review | 71, 73 | src/tools/constraints.rs |
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
- Pattern engine: `#[cfg(test)]` in `src/constraints/patterns.rs` (9 tests — rust/dart
  match, scope filtering, language filtering, enclosing symbol, identity propagation)
- Guard constraint filtering: `#[cfg(test)]` in `src/guard.rs` (14+ tests — severity
  filtering, waiver bypass, lightweight check, advisory passthrough, pattern
  introduced-only, pattern waiver bypass, pattern advisory passthrough, ratchet
  guard blocking + release-allows-edit)
- Ratchet: `tests/constraints-test.rs` (4 tests — drift detection on deletion,
  non-waivability, released-ratchet-inert, ratchet floor monotonicity);
  `tests/db-test.rs` (ratchet_upsert_and_get);
  `#[cfg(test)]` in `src/rules.rs` (per_constraint_ratchet_flag, defaults_false)
- Test engine setup: `DdEngine::new(Duration::from_secs(1800))`, no DB needed
- Test DB setup (waivers): `Db::open_unchecked("test", dir.path())` with tempdir
