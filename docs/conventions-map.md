# Convention system architecture map

Quick-reference for agents planning or implementing convention-system tasks.
Read this first, then do targeted `sutra_outline` / `sutra_read` calls on
specific files. Updated after each convention-system landing.

Last updated: 2026-07-08 (sutra/232: remove drift, template, waiver subsystems)

## Module layout

```
src/conventions/
  mod.rs            — re-exports, module declarations
  engine.rs         — FcaEngine, SymbolAttrs, Convention, ConventionViolation,
                      ConventionMatch, rebuild/check/check_inverse, dedup
  context.rs        — FormalContext, Implication, approximate_implications (FCA core)
  bitset.rs         — compact bitset for FCA attribute sets
  attributes.rs     — extract_attrs_for_symbol, extract_cross_language_attrs,
                      enrich_with_effects, EffectPattern, ResolvedCallee
  pipeline.rs       — rebuild (FCA pipeline: extract, rebuild, persist)

src/db/
  mod.rs            — Db struct, TABLE_REGISTRY, TablePartition, reindex,
                      file/symbol/ref CRUD
  conventions.rs    — ConventionRow, upsert/query/delete methods, fca_cache
  components.rs     — ComponentRow, insert/batch/active_components_with_paths,
                      anchors, aliases, component_lifecycle_state,
                      set_component_lifecycle
  migrations.rs     — MIGRATIONS array (name, sql, ephemeral_only), run_migrations

src/tools/
  review.rs         — handle (entry), build_findings (violation check),
                      compute (JSON assembly), ReviewFindings struct
  conventions.rs    — MCP tool action: list
  orient.rs         — sutra_orient MCP tool: convention-aware orientation
                      per scope. Resolves component, assembles observed
                      conventions and constraints. Core tier.
```

## Key types

### SymbolAttrs (engine.rs)
Input to FCA. Fields: `name`, `file`, `attributes: Vec<String>`, `component_id: Option<String>`.
Attributes are strings like `kind:function`, `vis:pub`, `has_doc`, `effect:fs`, `naming:snake_case`.

### Convention (engine.rs)
FCA output. Fields: `id` (blake3 hash), `antecedent`, `consequent` (both `Vec<String>`),
`support`, `confidence`, `component_id: Option<String>`.

### ReviewFindings (review.rs)
Aggregation passed from `build_findings` to `compute`. Fields:
`constraint_violations`, `convention_violations`,
`waived_constraint_violations`, `constraint_parse_errors`.

## Data flow: rebuild pipeline (pipeline.rs)

```
rebuild(db, registry, workspace_root)
  |
  +-- Build all_sym_attrs (all files, all symbols,
  |     extract + enrich attributes)
  |
  +-- Assign component_id to each SymbolAttrs via file_to_component map
  |
  +-- FCA rebuild (skip if hash matches cached):
  |     Global: FcaEngine::rebuild(all_sym_attrs)
  |     Per-component: rebuild_with_params(comp_symbols, adaptive_threshold)
  |       + deduplicate_component_conventions vs global
  |     --> all_convs
  |
  +-- Convention persistence:
  |     upsert all_convs, delete stale conventions

build_findings (review.rs):
  +-- Constraint evaluation via check::evaluate
  +-- Convention violation check: FcaEngine::check(changed_sym_attrs)
  --> ReviewFindings

compute(db, workspace_root, changed_paths, churn, findings)
  --> JSON with risk_score, violations, constraints
```

## Database tables (convention-related)

| Table | Partition | Migration | Purpose |
|---|---|---|---|
| conventions | Ephemeral | 0005+0007+0015 | FCA-discovered conventions (id, antecedent, consequent, support, confidence, component_id) |
| fca_cache | Ephemeral | 0040 | Hash of last FCA input to skip redundant rebuilds |
| components | Durable | 0008+0009+0021 | Component identity, lifecycle_state (stable/sketch) |
| component_membership | Ephemeral | 0008 | Component-to-file mapping (rebuilt on cluster) |

Dropped in 0044: convention_state, convention_proposals, convention_history.
Dropped in 0045: convention_waivers, convention_snapshots, convention_templates, drift_alerts.
Enforcement of naming/style rules is now via constraint rules in `.sutra/rules.toml`.

Ephemeral = dropped on reindex, migration re-runs. Durable = survives reindex.

## Migration patterns

- Sequential numbering: `NNNN_name.sql`
- Register in `src/db/migrations.rs` MIGRATIONS array: `(name, sql, ephemeral_only)`
- Add to TABLE_REGISTRY in `src/db/mod.rs` if creating a new table
- Ephemeral tables: `ephemeral_only: true`, `TablePartition::Ephemeral`
- Durable tables: `ephemeral_only: false`, `TablePartition::Durable`
- ALTER TABLE on durable table: always `ephemeral_only: false`
- Timestamps: `TEXT NOT NULL DEFAULT (datetime('now'))` in SQL, ISO-8601 via chrono in Rust

## DB method patterns

All methods on `impl Db` in submodules. Typical signature:
```rust
pub fn do_thing(&self, arg: &str) -> Result<ReturnType> {
    let conn = self.conn.lock();
    // conn.execute / conn.prepare + query_map
    // Error: ? propagates rusqlite::Error via SutraError::Db
    // Single-row not found: match QueryReturnedNoRows -> Ok(None) or default
}
```

Row types: plain `#[derive(Debug, Clone)]` structs, mapped via closure or helper fn.

## Sketch mode (ADR-0001)

Component lifecycle_state column: `stable` (default) or `sketch`.
When sketch: conventions are informational only, constraints remain enforced.
Currently only settable via `Db::set_component_lifecycle` — no MCP tool yet.

## FCA thresholds

- Global: MIN_SUPPORT=3, MIN_CONFIDENCE=0.9
- Per-component: adaptive `max(2, ceil(component_symbol_count * 0.4))`, capped at MAX_COMPONENT_SUPPORT=20
- Component conventions subsumed by global at equal/higher confidence are dropped

## Design docs

- PRD: yojana task `sutra/57` (not a file on disk)
- ADR-0001: `docs/adr/0001-sketch-mode-conventions-only.md`
- ADR-0002: `docs/adr/0002-ephemeral-durable-partition.md`
- ADR-0003: `docs/adr/0003-layered-adapter-traits.md`
- Domain glossary: `CONTEXT.md`
- Core model PRD: `docs/prd-core-model.md`

## Test locations

- Unit tests: `#[cfg(test)]` modules in `engine.rs`
- Integration tests: `tests/review-test.rs`, `tests/db-test.rs`
- Test DB setup: `Db::open_unchecked("test", dir.path())` with tempdir
