# Enforcement ledger

Tracks every architectural constraint: when it was introduced, why, current
status, and maintenance history. Backfilled from `.sutra/rules.toml` provenance
at checkpoint:sutra/175 (2026-06-16).

## Constraint inventory

### Guard isolation (blocking)

| Constraint | From | To | Provenance | Status |
|---|---|---|---|---|
| guard-no-conventions | src/guard.rs | src/conventions/* | ADR-0001 | live |
| guard-no-similarity | src/guard.rs | src/similarity/* | initial | live |
| guard-no-health | src/guard.rs | src/health/* | initial | live |
| guard-no-pipeline | src/guard.rs | src/pipeline.rs | initial | live |
| guard-no-tools | src/guard.rs | src/tools/* | initial | live |
| guard-bin-no-conventions | src/bin/guard.rs | src/conventions/* | initial | live |
| guard-bin-no-similarity | src/bin/guard.rs | src/similarity/* | initial | live |
| guard-bin-no-health | src/bin/guard.rs | src/health/* | initial | live |

Rationale: guard binary runs as a PreToolUse hook on every edit. Must stay
lightweight: read-only SQLite queries + rules TOML parsing. No analysis engines.

### DD worker purity (blocking)

| Constraint | From | To | Provenance | Status |
|---|---|---|---|---|
| worker-no-db | src/constraints/worker.rs | src/db/* | ADR-0002 | live |
| worker-no-tools | src/constraints/worker.rs | src/tools/* | initial | live |
| worker-no-conventions | src/constraints/worker.rs | src/conventions/* | initial | live |
| worker-no-similarity | src/constraints/worker.rs | src/similarity/* | initial | live |

Rationale: timely/DD worker thread is pure computation over crossbeam channels.
Ephemeral data is recomputable; worker needs no persistence (ADR-0002).

### Layer boundaries (blocking)

| Constraint | From | To | Provenance | Status |
|---|---|---|---|---|
| db-no-tools | src/db/* | src/tools/* | initial | live |
| parser-no-tools | src/parser/* | src/tools/* | initial | live |
| dd-engine-no-conventions | src/constraints/engine.rs | src/conventions/* | initial | live |
| conventions-no-constraints | src/conventions/* | src/constraints/* | initial | live |

Rationale: data layer must not import presentation. Parser (L0) must not import
tools. Constraint and convention engines are independent L2/L3 systems joined
only at the pipeline/tool level.

### Cycle prevention (blocking)

| Constraint | Scope | Provenance | Status |
|---|---|---|---|
| no-tool-cycles | src/tools/ | initial | live |
| no-constraint-cycles | src/constraints/ | initial | live |
| no-convention-cycles | src/conventions/ | initial | live |

### Known cross-layer coupling (advisory)

| Constraint | From | To | Provenance | Status | Violations |
|---|---|---|---|---|---|
| parser-conventions-coupling | src/parser/* | src/conventions/* | ADR-0003 | live | 1 (adapter.rs -> mod.rs) |
| similarity-health-coupling | src/similarity/* | src/health/* | initial | live | 1 (diff.rs -> findings.rs) |
| similarity-parser-coupling | src/similarity/* | src/parser/* | initial | live | 2 (diff.rs, mod.rs -> adapter.rs) |

These document existing architectural debt. Not blocking because they exist for
pragmatic reasons. Agents should not deepen them.

### Fan-in limits (advisory)

| Constraint | Target | Threshold | Provenance | Status |
|---|---|---|---|---|
| rules-fan-in | src/rules.rs | 15 | initial | live |
| pipeline-fan-in | src/pipeline.rs | 20 | initial | live |
| components-fan-in | src/components/mod.rs | 15 | initial | live |

### Lessons isolation (blocking, checkpoint:sutra/152)

| Constraint | From | To | Status |
|---|---|---|---|
| lessons-no-db | src/lessons.rs | src/db/* | live |
| guard-no-lessons | src/guard.rs | src/lessons.rs | live |
| worker-no-lessons | src/constraints/worker.rs | src/lessons.rs | live |

Rationale: LessonsDb owns its own SQLite file, independent of per-workspace DBs.
Heavyweight subsystems (guard, DD worker) must not depend on it.

### Lessons engine isolation (blocking, checkpoint:sutra/157)

| Constraint | From | To | Status |
|---|---|---|---|
| lessons-no-tools | src/lessons.rs | src/tools/* | live |
| lessons-no-conventions | src/lessons.rs | src/conventions/* | live |
| lessons-no-constraints | src/lessons.rs | src/constraints/* | live |

Rationale: lessons engine is a library consumed by tool handlers. Must not couple
back to tools, conventions (FCA), or constraints (DD). Complements the existing
lessons-no-db constraint from checkpoint:sutra/152.

### Explore tool isolation (blocking, checkpoint:sutra/171 + sutra/175)

| Constraint | From | To | Provenance | Status |
|---|---|---|---|---|
| explore-no-similarity | src/tools/explore.rs | src/similarity/* | checkpoint:sutra/171 | live |
| explore-no-conventions | src/tools/explore.rs | src/conventions/* | checkpoint:sutra/171 | live |
| explore-no-health | src/tools/explore.rs | src/health/* | checkpoint:sutra/175 | live |
| explore-no-lessons | src/tools/explore.rs | src/lessons.rs | checkpoint:sutra/175 | live |

Rationale: sutra_explore is a deterministic, index-only pipeline (grep -> rank ->
assemble). It ranks via db rollups (fan_in, blast_radius), not health findings or
lessons. The PRD explicitly rejects vector search, embeddings, and NL interpretation.

## Conventions

| Convention | Pattern | Lifecycle | Note |
|---|---|---|---|
| d274ef940edda9a4 | kind:const -> naming:SCREAMING | preferred | Rust standard |
| 8 IDs in suppress list | component-scoped tautologies | suppressed | "in:X -> in:X" patterns |

## Maintenance log

| Date | Checkpoint | Actions |
|---|---|---|
| 2026-06-16 | sutra/152 | 3 lessons-isolation constraints added |
| 2026-06-16 | sutra/171 | 2 explore-isolation constraints added, 8 tautological conventions suppressed |
| 2026-06-16 | sutra/175 | 2 explore constraints added (health, lessons). 15 convention proposals dismissed (all tautologies). Enforcement ledger backfilled from rules.toml provenance. |
| 2026-06-16 | sutra/157 | 3 lessons-engine isolation constraints added (no-tools, no-conventions, no-constraints). 29 orphaned convention proposals dismissed (re-clustered IDs). |
