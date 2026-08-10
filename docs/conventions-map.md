# Convention system architecture map

Quick-reference for agents planning or implementing convention-system tasks.
Read this first, then do targeted `sutra_outline` / `sutra_read` calls on
specific files. Updated after each convention-system landing.

Last updated: 2026-08-10 (sutra/312: deleted sutra_orient + describe_patterns/ObservedPattern)

## Module layout

```
src/conventions/
  mod.rs            — re-exports, module declarations
  engine.rs         — FcaEngine, SymbolAttrs, Convention, Deviation,
                      ConventionViolation (legacy, used by check),
                      rebuild/check/check_inverse, dedup,
                      detect_deviations (on-the-fly review path)
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
                      anchors, aliases, component_lifecycle_state,
                      set_component_lifecycle

src/tools/
  review.rs         — handle (entry), build_findings (deviation detection),
                      compute (JSON assembly), ReviewFindings struct,
                      Deviation re-export
  conventions.rs    — MCP tool action: list
```

## Key types

### SymbolAttrs (engine.rs)
Input to FCA. Fields: `name`, `file`, `attributes: Vec<String>`, `component_id: Option<String>`.
Attributes are strings like `kind:function`, `vis:pub`, `has_doc`, `effect:fs`, `naming:snake_case`.

### Convention (engine.rs)
FCA output. Fields: `id` (blake3 hash), `antecedent`, `consequent` (both `Vec<String>`),
`support`, `confidence`, `component_id: Option<String>`.

### Deviation (engine.rs)
Review-time finding. Fields: `symbol`, `file`, `pattern_antecedent`, `pattern_consequent`,
`missing`, `support`, `confidence`, `total_matching`, `conforming`, `exemplars`,
`strength` (support × confidence), `informational` (true for sketch-mode components).

### AttributeRole (attributes.rs)
Classification: `Identity` (antecedent-only, e.g. `vis:pub`, `kind:function`) vs
`Obligation` (consequent-checkable, e.g. `has_doc`, `returns_result`, `naming:*`, `effect:*`).

### ReviewFindings (review.rs)
Aggregation passed from `build_findings` to `compute`. Fields:
`constraint_violations`, `deviations`,
`waived_constraint_violations`, `constraint_parse_errors`.

## Data flow

### Review-time deviation detection (build_findings → compute)

```
build_findings(db, workspace_root, changed_paths, base_revision, dd_engine, registry)
  |
  +-- Constraint evaluation via check::evaluate
  |
  +-- Extract SymbolAttrs for ALL files, grouped by component
  |     (same enrich_all_effects pipeline as pipeline.rs)
  |
  +-- Identify changed symbols (subset of above)
  |
  +-- detect_deviations(changed_sym_attrs, all_by_component, orphans,
  |     toolchain_pairs, sketch_components)
  |     Per component:
  |       1. Build FormalContext from component siblings
  |       2. approximate_implications (includes confidence=1.0)
  |       3. Filter: consequent must be Obligation, not toolchain-enforced
  |       4. For each changed symbol matching antecedent but missing consequent:
  |          → Deviation with counts, exemplars, strength
  |     Rank by strength (support × confidence), cap at 5
  |
  --> ReviewFindings { constraint_violations, deviations, ... }

compute(db, workspace_root, changed_paths, churn, findings)
  --> JSON with risk_score, deviations, constraints
```

### Rebuild pipeline (pipeline.rs — persists to DB for sutra_conventions tool)

```
rebuild(db, registry, workspace_root)
  → FCA over all symbols (global + per-component)
  → Persists conventions to DB (conventions table)
  → Used by sutra_conventions tool, NOT by review
```

## Output contract (sutra_review JSON)

The `deviations` array replaces the former `convention_violations`. Each entry:
```json
{
  "symbol": "core::process",
  "file": "src/core.rs",
  "pattern": "kind:function, vis:pub → has_doc",
  "missing": ["has_doc"],
  "evidence": "8/10 siblings have has_doc",
  "exemplars": ["helper::format", "util::parse"],
  "support": 8,
  "confidence": 0.95,
  "strength": 7.6,
  "informational": false
}
```

`risk_breakdown.deviations` replaces `risk_breakdown.convention_violations`.
Informational deviations (sketch-mode components) do not contribute to risk score.

## Database tables (convention-related)

| Table | Partition | Migration | Purpose |
|---|---|---|---|
| conventions | Ephemeral | 0005+0007+0015 | FCA-discovered conventions — persisted by pipeline.rs for `sutra_conventions` list |
| fca_cache | Ephemeral | 0040 | Hash of last FCA input to skip redundant rebuilds |
| components | Durable | 0008+0009+0021 | Component identity, lifecycle_state (stable/sketch) |
| component_membership | Ephemeral | 0008 | Component-to-file mapping (rebuilt on cluster) |

Review does not read from the conventions table — it computes on-the-fly.
The conventions table survives for the `sutra_conventions` tool (list action).

## Sketch mode (ADR-0001)

Component lifecycle_state column: `stable` (default) or `sketch`.
When sketch: deviations are reported with `informational: true` and excluded from
risk score. Constraints remain enforced regardless of sketch mode.

## FCA thresholds

- Global: MIN_SUPPORT=3, MIN_CONFIDENCE=0.9 (includes confidence=1.0)
- Per-component: adaptive `max(2, ceil(component_symbol_count * 0.4))`, capped at MAX_COMPONENT_SUPPORT=20
- Direction filter: only identity→obligation implications are checkable
- Toolchain-enforced pairs (e.g. `is_async → returns_future`) excluded via adapter declarations
- MAX_DEVIATIONS=5 per review

## Design docs

- PRD: yojana task `sutra/57` (not a file on disk)
- ADR-0001: `docs/adr/0001-sketch-mode-conventions-only.md`
- ADR-0002: `docs/adr/0002-ephemeral-durable-partition.md`
- ADR-0003: `docs/adr/0003-layered-adapter-traits.md`
- Domain glossary: `CONTEXT.md`
- Core model PRD: `docs/prd-core-model.md`

## Test locations

- Unit tests: `#[cfg(test)]` modules in `engine.rs`, `context.rs`
- Integration tests: `tests/review-test.rs`, `tests/explain-test.rs`, `tests/db-test.rs`
- Test DB setup: `Db::open_unchecked("test", dir.path())` with tempdir
