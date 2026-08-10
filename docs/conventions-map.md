# Convention system architecture map

Quick-reference for agents planning or implementing convention-system tasks.
Read this first, then do targeted `sutra_outline` / `sutra_read` calls on
specific files. Updated after each convention-system landing.

Last updated: 2026-08-10 (sutra/318: FCA convention detection is now list-only.
The in-loop consumers were retired — the review deviation report (sutra/313)
and `sutra_orient`'s descriptive pattern summary (sutra/312) — and the dead
lifecycle apparatus was removed (sutra/318): sketch mode + the convention
deprecated/forbidden lifecycle (`check_inverse`/`ConventionMatch`). What remains:
FCA still mines conventions and persists them for `sutra_conventions(list)`.)

## Current shape (post 312/313/318)

FCA convention **detection** survives and runs in the parse pipeline; it
persists discovered conventions to the DB for the `sutra_conventions(list)`
tool. Everything that once consumed conventions in-loop is gone:

- `sutra_review` no longer computes or emits `deviations` (sutra/313). It has
  zero convention involvement now.
- `sutra_orient` (and its on-the-fly `describe_patterns` / `ObservedPattern`
  descriptive summary) was deleted (sutra/312).
- Sketch mode (the `components.lifecycle_state` column + accessors) and the
  `check_inverse`/`ConventionMatch` deprecated/forbidden path were removed as
  dead code (sutra/318) — both were orphaned once the deviation report went.

So the live convention surface is a single passive lister. The FCA core
(`FormalContext`, attribute extraction) is deliberately kept intact for
possible future constraint-authoring use.

Vestigial-but-untouched: the `convention_overrides` table (migration 0007,
the deprecated/forbidden/preferred store) has no reader after sutra/318 — a
candidate for a later cleanup migration, left in place for now.

## Module layout

```
src/conventions/
  mod.rs            — re-exports, module declarations, SymbolAttrs
  engine.rs         — FcaEngine, Convention,
                      rebuild/rebuild_with_params/update_incremental,
                      conventions(), component_min_support,
                      deduplicate_component_conventions
  context.rs        — FormalContext, Implication, approximate_implications,
                      count_with_attrs, exemplars_for (FCA core)
  bitset.rs         — compact bitset for FCA attribute sets
  attributes.rs     — extract_attrs_for_symbol, extract_cross_language_attrs,
                      enrich_with_effects, EffectPattern, ResolvedCallee,
                      AttributeRole (Identity/Obligation), classify_attribute
  pipeline.rs       — rebuild (FCA pipeline: extract, rebuild, persist to DB)

src/db/
  mod.rs            — Db struct, TABLE_REGISTRY, TablePartition, reindex,
                      file/symbol/ref CRUD
  conventions.rs    — ConventionRow, upsert/query/delete methods, fca_cache
  components.rs     — ComponentRow, insert/batch/active_components_with_paths,
                      anchors, aliases

src/tools/
  conventions.rs    — MCP tool action: list (the only convention consumer)
```

## Key types

### SymbolAttrs (mod.rs)
Input to FCA. Fields: `name`, `file`, `attributes: Vec<String>`, `component_id: Option<String>`.
Attributes are strings like `kind:function`, `vis:pub`, `has_doc`, `effect:fs`, `naming:snake_case`.

### Convention (engine.rs)
FCA output. Fields: `id` (blake3 hash), `antecedent`, `consequent` (both `Vec<String>`),
`support`, `confidence`, `component_id: Option<String>`.

### AttributeRole (attributes.rs)
Classification: `Identity` (antecedent-only, e.g. `vis:pub`, `kind:function`) vs
`Obligation` (consequent-checkable, e.g. `has_doc`, `returns_result`, `naming:*`, `effect:*`).

## Data flow

### Rebuild pipeline (pipeline.rs — the only live path)

```
rebuild(db, registry, workspace_root)
  → FCA over all symbols (global + per-component)
  → Persists conventions to DB (conventions table)
  → Consumed only by sutra_conventions(list)
```

`sutra_review` and the deleted `sutra_orient` used to compute conventions
on-the-fly; neither does now. Nothing reads conventions on-the-fly anymore.

## Output contract (sutra_conventions list)

`sutra_conventions(action="list")` returns the persisted conventions. Each entry
carries `id`, `antecedent`, `consequent`, `support`, `confidence`, and
`component_id`. There is no longer a `deviations` array anywhere in the tool
surface (removed from `sutra_review` in sutra/313).

## Database tables (convention-related)

| Table | Partition | Migration | Purpose |
|---|---|---|---|
| conventions | Ephemeral | 0005+0007+0015 | FCA-discovered conventions — persisted by pipeline.rs for `sutra_conventions` list |
| fca_cache | Ephemeral | 0040 | Hash of last FCA input to skip redundant rebuilds |
| components | Durable | 0008+0009 | Component identity (lifecycle_state column dropped in sutra/318, migration 0060) |
| component_membership | Ephemeral | 0008 | Component-to-file mapping (rebuilt on cluster) |

## FCA thresholds

- Global: MIN_SUPPORT=3, MIN_CONFIDENCE=0.9 (includes confidence=1.0)
- Per-component: adaptive `max(2, ceil(component_symbol_count * 0.4))`, capped at MAX_COMPONENT_SUPPORT=20
- Direction filter: only identity→obligation implications are checkable
- Toolchain-enforced pairs (e.g. `is_async → returns_future`) excluded via adapter declarations

## Design docs

- PRD: yojana task `sutra/57` (not a file on disk)
- ADR-0001: `docs/adr/0001-sketch-mode-conventions-only.md` (superseded — sketch mode removed in sutra/318)
- ADR-0002: `docs/adr/0002-ephemeral-durable-partition.md`
- ADR-0003: `docs/adr/0003-layered-adapter-traits.md`
- Domain glossary: `CONTEXT.md`
- Core model PRD: `docs/prd-core-model.md`

## Test locations

- Unit tests: `#[cfg(test)]` modules in `engine.rs`, `context.rs`
- Integration tests: `tests/review-test.rs`, `tests/explain-test.rs`, `tests/db-test.rs`
- Test DB setup: `Db::open_unchecked("test", dir.path())` with tempdir
