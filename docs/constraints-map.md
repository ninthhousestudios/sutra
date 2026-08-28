# Constraint system architecture map

Quick-reference for agents planning or implementing constraint-system tasks.
Read this first, then do targeted `sutra_outline` / `sutra_symbol` calls on
specific files. Updated after each constraint-system landing.

Last updated: 2026-08-28 (sutra/360: import cycles acceptable by file-set — see "Cycle acks by file-set"; sutra/309: accepted.toml freshness gate; sutra/297: the shared DD engine resyncs its graph on every evaluation — see "Session-lifetime graph staleness")

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
                      ingest, sync_edges, update, set_forbidden_pairs,
                      query_violations, query_cycles, query_blast_radius[_all],
                      evict_if_idle.
                      query_forbidden_deps (deprecated, no callers).
                      sync_edges reconciles the cached graph with the index's
                      current edge set and is what every evaluation calls (see
                      "Session-lifetime graph staleness"). apply_edge_delta is
                      the one path that mutates edges — shared by sync_edges and
                      update, and it commits the tracked list only after the
                      worker acknowledges.
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
                      location + enclosing symbol resolution. Matches inside
                      test-only line ranges are dropped unless the constraint
                      sets include_tests (see "Test scope"). No DD involvement
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
  accepted.rs       — `.sutra/accepted.toml` source-of-truth layer (sutra/303,
                      sutra/308). Types: AcceptedFile (waivers + acks arrays),
                      WaiverEntry, AckEntry, AcceptedWarning, AcceptedLoad,
                      RefResolution. Public API: refresh_cache (read-surface
                      gate: migrate_db_to_file THEN ensure_cache_fresh),
                      is_cache_fresh_conn (guard-side read-only freshness
                      check), resolve_waivers_for_guard (in-memory fallback
                      when cache stale), upsert_waiver / remove_waiver /
                      upsert_ack / remove_ack (file writers),
                      write_accepted_file, load_accepted_file,
                      resolve_accepted, current_file_hash.
                      See "Accepted.toml freshness gate" section below.
  check.rs          — Unified constraint evaluation. evaluate() dispatches to
                      evaluate_dd (DD-backed: review, sutra_constraints
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
                      confined_external's manifest signal is skipped for the
                      package that owns an allowed_in path (manifest_owns_
                      confinement, sutra/291) — otherwise a single-crate rule is
                      unsatisfiable, since Cargo.toml is never in allowed_in.
                      Ownership is decided by component-wise glob alignment
                      (pattern_reach), not literal-prefix arithmetic: for
                      crates/*/src/db.rs the literal head crates/ is no package,
                      so a prefix rule exempts the root and still blocks the
                      member — inverted. A nested package the pattern also
                      reaches takes the path; a leading ** reaches every package
                      including the declarer; ambiguity resolves to NOT owning,
                      since a wrong exemption silently disables a blocking rule.
                      Guard and index both derive package dirs from disk
                      (package_dirs_including) — a members-derived guard view
                      disagreed with the index on nested non-member packages.
                      The skip is match applicability, not a post-filter.
  worker.rs         — timely/DD worker thread, Command/Response enums,
                      WorkerHandle, spawn_worker, run_worker (dataflow +
                      command loop), Kosaraju SCC

src/rules.rs        — TOML parsing for .sutra/rules.toml.
                      Types: Severity, ConstraintKind, Constraint, RawConstraint,
                      Rules, Constraints, ForbiddenDep, ConventionsConfig.
                      Functions: parse_rules, load_rules, Rules::all_constraints,
                      scope_matches_path (hybrid glob-or-prefix scope matching,
                      used by match_no_cycles_constraint, constraint_coverage),
                      match_no_cycles_constraint.

src/db/
  constraints.rs    — ConstraintWaiverRow, CRUD for constraint_waivers table
                      (now a reproject-on-read cache of `.sutra/accepted.toml`;
                      see "Accepted.toml freshness gate"). Row ids are
                      re-minted on every sync, so they are not stable handles.
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
  constraints.rs    — MCP tool: sutra_constraints. Actions: list (all
                      constraints with metadata, waiver counts,
                      matched_file_count per field, dead-constraint warning),
                      violations (DD maintained view — forbidden_dep, boundary,
                      no_cycles, max_fan_in, forbidden_external, confined_external,
                      plus dead_constraint informational findings),
                      waive / unwaive (guard-honored waivers),
                      baseline / ack / unack (report-only instance acks).
                      Write actions write `.sutra/accepted.toml` + re-project
                      the DB cache (migrate_db_to_file → upsert/remove →
                      ensure_cache_fresh). Removal is key-based, not id-based
                      (projection re-mints ids on every sync).

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
`provenance: Option<String>`, `scope: Option<String>`, `ratchet: bool`,
`include_tests: bool`.

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
are excluded — name is an alias for human reference, not identity. `ratchet`
and `include_tests` are excluded too: they modulate enforcement, and toggling
them must not orphan waivers or ratchet registrations. Truncated to 8 hex
chars, matching convention ID style.

### DdEngine (engine.rs)
State machine: `Cold` → `Loaded { edges, forbidden_pairs }` → `Warm { handle,
edges, forbidden_pairs, last_query }`. Transitions:
- `ingest()`: Cold → Loaded (once only)
- `sync_edges()`: Cold → Loaded, or a delta against the existing graph
- `ensure_warm()`: Loaded → Warm (spawns worker, sends edges + forbidden pairs)
- `evict_if_idle()`: Warm → Loaded (preserves edges + forbidden pairs)
- Drop: → Cold (shuts down worker)

### ConstraintResolver (resolver.rs)
Resolves `Vec<Constraint>` to `Vec<(i64, i64)>` forbidden pairs. For
ForbiddenDep: glob-matches paths in the path_map. For Boundary: looks up
component membership via DB. Caches result keyed by `(input_hash,
clustering_generation)` — invalidate on component recompute. Used by
build_findings before calling `set_forbidden_pairs`.

### CheckOutcome (check.rs)
`{ active, waived, resolved, parse_errors, accepted_warnings }`.
`accepted_warnings: Vec<String>` surfaces operator-facing warnings from
resolving `.sutra/accepted.toml` against the live rule set (unknown/ambiguous
constraint refs). Populated only by the DD-backed report path (`evaluate_dd`);
the guard's `RawConn` path leaves it empty (config warnings belong on the
report, not at the edit-time gate).

### ConstraintWaiverRow (db/constraints.rs)
`{ id, constraint_id, constraint_name, file_path, symbol_qualified_name,
rationale, waived_by, created_at, updated_at }`. Now a **cache row** projected
from `.sutra/accepted.toml` — ids are re-minted on every sync, so they are not
stable handles. Waiver lookup in review: match on `constraint_id` +
`file_path` (either from_path or to_path).

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

Second invariant: the edge collection is a *set*. `Ingest`/`Update` feed DD only
when `edges_store` actually changed, because `Db::import_edges` repeats a pair
once per import statement — a duplicate insert would raise that pair's
multiplicity above the store's, and a later single retraction would leave the
edge alive in the dataflow alone (sutra/297).

## Session-lifetime graph staleness (sutra/297)

A shared `DdEngine` is per-workspace and per-server-session, so its graph has to
be reconciled with the index on every evaluation. `check.rs::evaluate_dd` calls
`engine.sync_edges(&edges)` unconditionally for exactly that reason; the diff is
usually empty and returns before touching the worker.

Staleness here does not degrade gracefully. `Db::replace_file_data` deletes and
re-inserts the `files` row on every reparse, and `files.id` is `AUTOINCREMENT`,
so a reparsed file gets a **brand-new id** — yojana's 25 files span ids 241–341.
Forbidden pairs are resolved fresh against live ids each evaluation, so a cached
graph doesn't merely miss an edge: it is disjoint from the pair set, the
violations semijoin returns empty, and *every* DD-backed constraint reports zero
with no distinction between "clean" and "not evaluated". `no_cycles` fails the
same way one step later — stale SCC node ids miss `path_map` and the cycle is
skipped.

The original design relied on an `invalidate()` signal that had **no callers**,
which is why this survived: the engine ingested once per session and was never
told otherwise. Sync-at-use replaces it (`invalidate`/`is_invalidated`/`reload`
are gone) — a check that can't be forgotten by a future writer of the index.
Only DD-backed kinds were affected; `forbidden_pattern` reads from disk and the
`external` kinds re-read `unresolved_imports` each call, which is why those held
steady in the field report.

## Accepted.toml freshness gate (sutra/303, sutra/308)

`.sutra/accepted.toml` is the **source of truth** for waivers (guard-honored,
coarse suppression) and instance acks (report-only, content-keyed,
count-aware). The DB tables `constraint_waivers` and `constraint_instance_acks`
are a reproject-on-read cache — row ids are re-minted on every sync and are
not stable handles. The file is version-controlled, so waivers and acks appear
in `git diff` as reviewable events.

### File format

```toml
[[waiver]]
constraint = "no-tool-daemon"       # constraint NAME (human-stable)
file = "src/tools/review.rs"
symbol = "build_findings"           # optional; absent = whole-file
rationale = "reviewed: legacy path"
by = "josh"

[[ack]]
constraint = "no-clone-driven-dev"  # constraint NAME
file = "src/parser/rust.rs"
symbol = "parse"                    # enclosing_symbol (MatchKey part)
snippet = ".clone()"                # matched node's first line (MatchKey part)
count = 2                           # how many matches of this key are accepted
rationale = "examined: unavoidable" # optional for baseline, required for ack
by = "josh"
```

Entries are keyed by constraint **name** (resolved to an id at load via
`resolve_ref`), not by the blake3 id. A constraint with no name can only be
referenced by its id, which surfaces as an `unknown` warning on load — the
signal to give the rule a name. Names survive a scope/param edit that reshapes
the blake3 id.

### Read-surface freshness gate

`accepted::refresh_cache(db, root, constraints)` is called at the head of
every DD-backed report path. Call sites:

| Surface | Where |
|---|---|
| `evaluate_dd` | `check.rs:177` — before any DB-backed waiver/ack read |
| `list` | `constraints.rs:89` — reads waiver cache directly, no `evaluate` |

`refresh_cache` does two things in load-bearing order:

1. **`migrate_db_to_file`** — one-time: dumps any pre-existing local-only DB
   waivers/acks into `accepted.toml` so the move to file-authoritative does not
   strand them. Gated on file absence (`Ok(None)` when the file already exists
   or there is nothing to migrate). **Must run before** `ensure_cache_fresh`,
   or a repo whose acceptances still live only in the DB (no file yet) would
   reproject from an absent → empty file and wipe them (sutra/308 hazard 1).

2. **`ensure_cache_fresh`** — parse + resolve the file (so warnings are always
   current), then re-project the DB cache only when the on-disk blake3 hash
   differs from the stored marker (`accepted_sync_marker`). The parse is
   microseconds; the freshness check gates only the DB *write*, not the read.

Returns `Vec<AcceptedWarning>` — unknown/ambiguous constraint refs surfaced on
the report so a waiver silently pointing at a deleted constraint is visible.
Mapped to `CheckOutcome.accepted_warnings` strings by all three call sites.

### Guard stale path

`evaluate_raw` (the guard's edit-time path) holds a bare `Connection`, not a
writable `Db`, so it cannot reproject. It uses `accepted::is_cache_fresh_conn`
to decide which waiver source to trust:

- **Cache fresh** (a server review already projected the current file):
  fast path — read `constraint_waivers` rows from the DB directly.
- **Cache stale** (a hand-edited `accepted.toml` no server pass has seen yet):
  call `accepted::resolve_waivers_for_guard(root, constraints)` — identical
  data derived in-memory from the same file the server-side reprojection would
  use, no persist, no writable handle on the latency path. Acks are
  report-only, so the guard never needs them.

This ensures the guard honors exactly what the next audit report will show
(guard must predict the report, sutra/308 hazard 3).

### Write actions

All write actions follow the same pattern:
`migrate_db_to_file` → file mutation → `ensure_cache_fresh`.

| Action | File mutation | Key |
|---|---|---|
| `waive` | `accepted::upsert_waiver` | `(constraint, file, symbol)` |
| `unwaive` | `accepted::remove_waiver` | `(constraint, file, symbol)` |
| `baseline` | `accepted::upsert_ack` (per content key) | `(constraint, file, symbol, snippet)` |
| `ack` | `accepted::upsert_ack` (single instance) | `(constraint, file, symbol, snippet)` |
| `unack` | `accepted::remove_ack` | `(constraint, file, symbol, snippet)` |
| `ack-cycle` | `accepted::upsert_ack` (one cycle) | `(constraint, file, snippet=file-set)` |
| `unack-cycle` | `accepted::remove_ack` | `(constraint, file, snippet=file-set)` |

`migrate_db_to_file` runs first in each — so any legacy DB-only rows are
seeded before the new entry is appended; skip it and an absent file would be
created carrying only the new entry, wiping the rest on re-projection.
`ensure_cache_fresh` at the end so the very next report/guard check sees the
change.

### Resolution and warnings

`resolve_accepted` resolves every file entry against the live constraints,
partitioning into projectable rows (`AcceptedLoad.waivers` + `.acks`) and
`AcceptedWarning`s for entries that did not resolve. Two warning kinds:

- **Unknown** — constraint name not found in the live rule set (deleted or
  renamed constraint). The entry is *not dropped* — it stays in the file so a
  rename can be fixed manually.
- **Ambiguous** — constraint name matches multiple live constraints (a name
  collision). Also preserved in the file; the operator must disambiguate.

Warnings are surfaced on every report surface (`accepted_warnings` field) and
via the `violations` tool output, never silently swallowed.

## Cycle acks by file-set (sutra/360)

An import cycle is accepted the same content-keyed way a `forbidden_pattern`
clone is — reusing the ack machinery whole rather than a parallel path. The
cycle's identity is its **sorted file-set**, and that set rides in the finding's
`snippet` (the `MatchKey` content part), so `apply_instance_acks` cancels exactly
that cycle while a reshaped one re-surfaces. sutra/359 first *demoted* un-owned
cycles from Blocking to Advisory (removing the phantom gate); this is the
per-instance acceptance lever that demotion deferred.

- **Fingerprint.** `check::cycle_fingerprint(&[&str])` sorts the members
  lexically, dedups, and joins with `" -> "`. Lexical (not id) order is
  load-bearing: file ids are reminted on every reparse (sutra/297), so an id
  order would make both the ack's storage bucket and the fingerprint unstable
  across sessions. The cycle finding sets `snippet = Some(fingerprint)` and
  `from_path`/`to_path` to the lexically first/last member (the ack bucket).
- **Ackable kinds.** `apply_instance_acks` partitions on
  `forbidden_pattern | no_cycles` (was pattern-only). Every other kind is
  waive-only and passes through untouched. Count-awareness is inherited but
  trivial for a cycle (a given file-set is one instance, `count = 1`); a member
  added or removed changes the set → a key miss → the cycle re-surfaces. This
  covers both an **un-owned** cycle (`constraint_id = builtin:cycles`) and one
  **owned** by a named `no_cycles` rule — the latter was previously only
  *waivable* (leaky from_path), never ackable.
- **Reserved `builtin:cycles` carve-out.** An un-owned cycle has no rules.toml
  entry, so its ack cannot resolve by name. `resolve_accepted`'s **ack branch**
  recognizes the reserved name `builtin:cycles` and projects it against the
  synthetic id directly. This is the **only** id-keyed acceptance — every other
  nameless ref still warns Unknown, so sutra/310's ban on general id-keyed
  entries holds. Deliberately **ack-only**: the waiver branch stays name-pure, so
  an un-owned cycle can never carry a leaky, guard-honored from_path waiver.
- **Write path.** `ack-cycle` takes `members` (the file set, order-insensitive),
  re-verifies it against the live report (a typo'd/stale set is rejected, not
  stranded as a phantom ack), reads the owned/un-owned constraint identity off
  the matching finding, and upserts `[[ack]] constraint = <name|builtin:cycles>,
  file = <lex-first member>, snippet = <fingerprint>, count = 1`. `unack-cycle`
  mirrors it, recovering the stored constraint key from the file so removal works
  for owned and builtin alike without the operator naming the constraint. A
  nameless *owned* `no_cycles` rule is still rejected (`require_named_constraint`),
  same as any ack.

`[[ack]]` for a cycle in `.sutra/accepted.toml`:

```toml
[[ack]]
constraint = "builtin:cycles"          # or the named no_cycles rule
file = "src/a.rs"                       # lexically first member (the bucket)
snippet = "src/a.rs -> src/b.rs"        # the sorted file-set — the identity
count = 1
rationale = "reviewed: idiomatic re-export cycle"
by = "josh"
```

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
include_tests = false            # optional, default false. See "Test scope".
```

## Test scope (sutra/290)

Test-only code is excluded from every constraint kind unless the constraint
sets `include_tests = true`. Three independent mechanisms, one flag:

**Line ranges (pattern kinds).** `LanguageAdapter::test_line_ranges(ctx)`
returns 1-based inclusive ranges; default impl is empty, so a language opts in
by overriding. `parser::rust::test_line_ranges` walks for `attribute_item`
siblings marking `#[cfg(test)]` / `#[test]` / `#[tokio::test]` and spans from
the attribute line through the end of the item it annotates. `cfg` predicates
are evaluated structurally by `cfg_predicate_is_test` (sutra/293), asking one
question: does the predicate hold *only* in a test build? `test` and
`all(test, ..)` do; `any(test, ..)` does not (a sibling operand can hold in
release), nor does any `not(..)`, `feature = "test-helpers"`, or `cfg_attr(test,
..)` — which gates a nested *attribute*, not the item. Everything unrecognised
falls through to production, so a misparse leaves a rule over-reporting rather
than silently muted. `patterns.rs` caches ranges per
path across the per-constraint loop and drops matches falling inside them.
`adapter::line_in_ranges` is the shared containment check.

**Test paths (all kinds, sutra/292 + sutra/295).** A whole-file test target
carries no attribute for line ranges to find, so
`LanguageAdapter::is_test_path(path)` classifies by convention. Every adapter
overrides it (default impl is `false`, so a new language opts in deliberately):

| language | classified as test |
| --- | --- |
| rust | `tests/`, `benches/` |
| dart | `test/`, `tests/`, `integration_test/`, `*_test.dart` |
| python | `test_*.py`, `*_test.py`, `test/`, `tests/` |
| c | `test_*`, `*_test.c`, `test/`, `tests/` |
| javascript / typescript | `*.test.*`, `*.spec.*`, `__tests__/`, `test/`, `tests/` |

`adapter::path_has_dir_segment` matches a *directory* component anywhere in the
path (so a monorepo's `crates/core/tests/` counts) and never the file name,
keeping `src/tests.rs` production. `adapter::path_in_test_dir` is the shared
`test/`-or-`tests/` check; Rust deliberately does not use it, because Cargo gives
`tests/` and `benches/` an exact meaning a bare `test/` lacks. `patterns.rs`
skips such a file wholesale.

Note the split between `is_test_path` and each language's older `is_test_file`:
the latter drives symbol `FLAG_TEST` and stays keyed on *file naming* only. A
directory says "not production code" without saying "every symbol under it is a
test", so wiring directories into symbol flags would overreach — python.rs, c.rs
and javascript.rs each keep both functions for that reason.

The escape hatch is the rule's own path globs: `scope = "tests/**"` keeps firing
inside `tests/`, because a rule aimed at test code would otherwise go silently
inert. Only a glob's *literal prefix* counts (`constraints::glob_targets_tests`)
— `**/*.rs` and an unscoped rule both want the default exclusion. `include_tests
= true` remains the way to cover production and tests at once.

`constraints::constraint_targets_tests` decides which globs a kind puts in play
(sutra/296): `scope` always, plus `forbidden_dep`'s `from`/`to` and
`forbidden_external`'s `from`. `confined_external`'s `allowed_in` is deliberately
excluded — it is an allowlist, so naming `tests/**` there says test usage is
*permitted*, the opposite of aiming the rule at tests. Component-named kinds
(`boundary`) carry no path of their own and rest on scope alone.

Which classifier answers `is_test_path` depends on what the constraint knows:
`forbidden_pattern` names a language, so it asks that adapter; dep, cycle and
external rules span the workspace, so they ask
`adapter::any_language_is_test_path` (true when *any* registered adapter says
so). A `tests/**` glob is not written against one grammar.

**Edge flag (dep kinds).** `imports.is_test` (migration 0053) is set at parse
time: `rust::parse` tests each import's line against the line ranges, and
`ParserPool::parse_with` flags every import in a file whose path `is_test_path`
— which is what gives Dart and Rust integration tests their edge behaviour.
`db::production_import_edges()` returns the pairs backed by at least one
non-test import. Note that `Graph::import_edges()` (pagerank, impact, blast
radius) is deliberately unfiltered — test exclusion is a constraint-evaluation
contract only.

- `check.rs::evaluate_dd` keeps the DD graph whole — blast radius and SCC
  discovery both want the full picture — and filters at *finding* time, which
  is what keeps `include_tests` per-constraint rather than per-graph.
- `forbidden_dep`/`boundary`: skip when the pair is absent from
  `production_import_edges`. The Resolved-delta path skips only when the pair
  is still present but now test-only; a pair gone from the graph entirely is a
  genuine resolution and is still reported. Both skips step aside for a
  test-directed constraint (`check.rs::test_directed_ids`, computed once per
  evaluation — the classifier is path-only, so it cannot vary per edge).
- `no_cycles`: re-runs `worker::compute_sccs` over production edges restricted
  to the reported cycle's nodes and emits the surviving sub-SCCs. A
  pure-production cycle round-trips unchanged (both paths sort node ids), a
  test-only cycle disappears, a mixed cycle narrows to its real core. Singleton
  SCCs survive only when production backs a self-edge: a self-import reaches
  `no_cycles` as a one-node SCC, so filtering all singletons would drop a real
  cycle (sutra/294). A cycle whose matched rule is test-directed is reported
  whole, without the production narrowing.
- `forbidden_external`/`confined_external`: `is_test` rides along on
  `db::UnresolvedImport`, and `check_import_items` matches a test item only
  against constraints that want it — `include_tests` (sutra/294) or
  test-directed (sutra/296). Applicability is part of *matching*
  (`match_external_where`), not a filter after it: external matching is
  first-match, so filtering afterwards would let a broad rule win the match and
  then discard the item, shadowing a narrower rule that would have fired
  (sutra/296). Findings stay one per `(file, crate)`. Manifest-derived findings
  are unaffected — `[dev-dependencies]` is already its own axis via
  `include_dev`.
- Guard *edge* paths (`evaluate_raw`, `guard::proposed edges`,
  `get_incoming_edges`) drop test edges unconditionally rather than honouring
  `include_tests` — the review path still enforces that case, and an edit-time
  deny on test wiring is the exact failure sutra/290 was filed for. Guard
  *externals* do carry the flag: the parser has already computed it, so
  per-constraint fidelity there costs nothing.

Known gaps: Dart's `@visibleForTesting` (a production symbol reserved for test
use) has no equivalent — only path classification applies there. Fixture
directories that avoid a test-named path (`resources/corpus/`, `testdata/`) are
still evaluated; sutra will not guess at a project-specific layout, and
`scope` is the answer there.

Migration 0053 defaults `is_test = 0`, and the column is only ever written at
parse time — but the pipeline skips a file whose stored `content_hash` still
matches disk, so an older index would have kept the old edge behaviour
indefinitely. Migration 0054 clears `content_hash` on Rust files, forcing one
reparse that repopulates `is_test` (sutra/293). Migration 0055 does the same for
Rust *and* Dart when path classification landed (sutra/292), and 0056 for
Python, C and JS/TS (sutra/295). Pattern kinds read from disk and take effect
immediately.

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
| sutra/75 | orient constraint section | done (orient later deleted, sutra/312) | 70, 73 | ~~src/tools/orient.rs~~ |
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

- Accepted.toml: `#[cfg(test)]` in `src/constraints/accepted.rs` (9 tests —
  roundtrip, absent-file, malformed-file, unknown-constraint warning,
  cache freshness + idempotency, edited-file reprojects, guard-resolution
  matches server, upsert-replaces-not-appends, builtin:cycles carve-out resolves
  while other nameless refs still warn)
- Cycle acks by file-set (sutra/360): `#[cfg(test)]` in `src/tools/constraints.rs`
  (3 tests — un-owned cycle ackable + persists + unack restores + phantom-set
  rejected, reshaped cycle re-surfaces past an ack, owned named cycle ackable by
  file-set)
- Unit tests: `#[cfg(test)]` in `src/rules.rs` (22 tests — parsing, identity, defaults, errors)
- Integration tests: `tests/constraints-test.rs` (27 tests — cycles, blast radius,
  forbidden deps ad-hoc, maintained violations, eviction/rewarm)
- Review integration: `tests/review-test.rs` (22 tests — maintained view, waiver partition,
  delta labels, enriched violation fields, compute serialization)
- Pattern engine: `#[cfg(test)]` in `src/constraints/patterns.rs` (13 tests — rust/dart
  match, scope filtering, language filtering, enclosing symbol, identity propagation,
  cfg(test) exclusion, include_tests opt-in, bare #[test] attrs, cfg(not(test)) safety)
- Test scope, edge side: `tests/constraints-test.rs` (4 tests — test-only cycle
  suppressed, production cycle survives alongside test edges, include_tests restores
  the cycle, test-only forbidden_dep suppressed);
  `#[cfg(test)]` in `src/parser/rust.rs` (import is_test flagging, range spans)
- Guard constraint filtering: `#[cfg(test)]` in `src/guard.rs` (14+ tests — severity
  filtering, waiver bypass, lightweight check, advisory passthrough, pattern
  introduced-only, pattern waiver bypass, pattern advisory passthrough, ratchet
  guard blocking + release-allows-edit)
- Session staleness: `tests/constraints-test.rs` (4 tests — forbidden_dep +
  forbidden_external across five id-reminting cycles, no_cycles across three,
  survival of a rules.toml reload, exact retraction of a duplicated edge);
  `tests/review-test.rs` (build_findings resyncs a shared engine holding a
  stale graph)
- Ratchet: `tests/constraints-test.rs` (4 tests — drift detection on deletion,
  non-waivability, released-ratchet-inert, ratchet floor monotonicity);
  `tests/db-test.rs` (ratchet_upsert_and_get);
  `#[cfg(test)]` in `src/rules.rs` (per_constraint_ratchet_flag, defaults_false)
- Test engine setup: `DdEngine::new(Duration::from_secs(1800))`, no DB needed
- Test DB setup (waivers): `Db::open_unchecked("test", dir.path())` with tempdir
