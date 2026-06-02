# Convention system architecture map

Quick-reference for agents planning or implementing convention-system tasks.
Read this first, then do targeted `sutra_outline` / `sutra_read` calls on
specific files. Updated after each convention-system landing.

Last updated: 2026-06-02 (5c-8: structural convention templates)

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
  lifecycle.rs      — detect_signals, generate_proposals (N=3 trend window)
  drift.rs          — shannon_entropy, compute_attribute_distribution,
                      record_and_detect_drift, DriftAlert, DivergingAttribute
  templates.rs      — SymbolSignatureInfo, decompose_signature,
                      select_exemplars, generate_template,
                      generate_templates_for_conventions

src/db/
  mod.rs            — Db struct, TABLE_REGISTRY, TablePartition, reindex,
                      file/symbol/ref CRUD
  conventions.rs    — ConventionRow, ConventionStateRow, ConventionWithState,
                      ConventionHistoryRow, ConventionProposalRow,
                      ConventionWaiverRow, ConventionSnapshotRow,
                      ConventionTemplateRow,
                      upsert/query/history/proposal/waiver/snapshot/template methods
  components.rs     — ComponentRow, insert/batch/active_components_with_paths,
                      anchors, aliases, component_lifecycle_state,
                      set_component_lifecycle
  migrations.rs     — MIGRATIONS array (name, sql, ephemeral_only), run_migrations

src/tools/
  review.rs         — handle (entry), build_findings (FCA rebuild + violation
                      check + drift detection + template generation),
                      compute (JSON assembly), ReviewFindings struct
  conventions.rs    — MCP tool actions: list, violations, promote, demote,
                      waiver CRUD, proposals
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
`constraint_violations`, `convention_violations`, `convention_matches`,
`waived_violations`, `drift_alerts`.

### DriftAlert (drift.rs)
Fields: `component_id`, `component_name`, `entropy_old`, `entropy_new`,
`delta`, `diverging_attributes: Vec<DivergingAttribute>`.

## Data flow: review pipeline

```
build_findings(db, workspace_root, changed_paths, dd_engine, registry)
  |
  +-- DD engine: forbidden deps + cycles --> constraint_violations
  |
  +-- Build all_sym_attrs (all files, all symbols, extract + enrich attributes)
  |
  +-- Assign component_id to each SymbolAttrs via file_to_component map
  |
  +-- FCA rebuild:
  |     Global: FcaEngine::rebuild(all_sym_attrs)
  |     Per-component: rebuild_with_params(comp_symbols, adaptive_threshold)
  |       + deduplicate_component_conventions vs global
  |     --> all_convs
  |
  +-- Drift detection:
  |     record_and_detect_drift(db, comp_symbol_groups)
  |       Per component (skip if sketch mode):
  |         compute_attribute_distribution -> shannon_entropy
  |         detect_drift (check last 2 snapshots + current: monotonic, delta > 0.15)
  |         insert_convention_snapshot
  |     --> drift_alerts
  |
  +-- Convention persistence:
  |     upsert all_convs, record_convention_history(snapshot_id)
  |     detect_signals -> generate_proposals (lifecycle transitions)
  |     delete stale conventions
  |
  +-- Template generation:
  |     generate_templates_for_conventions(all_convs, all_sym_attrs, sig_info)
  |       Per convention (skip if support < 3):
  |         select_exemplars (rank by coverage, median complexity, recency)
  |         decompose_signature (parse sig text + language_attrs)
  |         generate_template (common parts literal, varying → metavariables)
  |       upsert_convention_template, delete_orphan_templates
  |
  +-- Violation check:
  |     FcaEngine::check(changed_sym_attrs) --> convention_violations
  |     FcaEngine::check_inverse(deprecated/forbidden) --> convention_matches
  |
  +-- Waiver partition:
        waivers_for_check() -> split violations into waived vs unwaived

compute(db, workspace_root, changed_paths, churn, findings)
  --> JSON with risk_score, violations, matches, waivers, drift_alerts
```

## Database tables (convention-related)

| Table | Partition | Migration | Purpose |
|---|---|---|---|
| conventions | Ephemeral | 0005+0007+0015 | FCA-discovered conventions (id, antecedent, consequent, support, confidence, component_id) |
| convention_state | Durable | 0014 | Human-set lifecycle (convention_id, lifecycle_state, override_reason) |
| convention_history | Ephemeral | 0016 | Per-convention support/confidence per snapshot |
| convention_proposals | Durable | 0017+0019 | Lifecycle transition proposals (pending/accepted/dismissed) |
| convention_waivers | Durable | 0018 | Waived violations with rationale |
| convention_snapshots | Ephemeral | 0020 | Per-component entropy snapshots for drift detection |
| convention_templates | Ephemeral | 0022 | Per-convention signature skeletons with exemplar symbols |
| components | Durable | 0008+0009+0021 | Component identity, lifecycle_state (stable/sketch) |
| component_membership | Ephemeral | 0008 | Component-to-file mapping (rebuilt on cluster) |

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

## Convention lifecycle

States: descriptive -> preferred -> deprecated -> forbidden.
All transitions human-initiated or proposed + confirmed.
Signals detected in `lifecycle::detect_signals` using N=3 snapshot trend window.
Proposals stored in `convention_proposals` (pending/accepted/dismissed).

## Sketch mode (ADR-0001)

Component lifecycle_state column: `stable` (default) or `sketch`.
When sketch: conventions flatten to informational, constraints remain enforced.
Drift detection skips sketch-mode components entirely (no snapshots recorded).
Currently only settable via `Db::set_component_lifecycle` — no MCP tool yet.

## FCA thresholds

- Global: MIN_SUPPORT=3, MIN_CONFIDENCE=0.9
- Per-component: adaptive `max(2, ceil(component_symbol_count * 0.4))`, capped at MAX_COMPONENT_SUPPORT=20
- Component conventions subsumed by global at equal/higher confidence are dropped

## Drift detection

- DRIFT_THRESHOLD = 0.15, DRIFT_WINDOW = 3
- Shannon entropy of per-component attribute distribution
- Alert when: net increase > 0.15 AND monotonically non-decreasing across window
- Diverging attributes: those whose proportion moved closer to 0.5
- Snapshots stored with full distribution JSON + blake3 hash
- Query orders by `snapshot_ts DESC, id DESC` (tiebreaker for sub-second inserts)

## Structural templates

- Generated per convention with support ≥ 3 and ≥ 2 decomposable exemplars
- Exemplar ranking: coverage (extra attrs beyond antecedent∪consequent) > median complexity > recency (index order)
- Signature decomposition: parses `SymbolRow.signature` text + `language_attrs` JSON into visibility, async/unsafe, generics, params, return type
- Metavariable rules: `$NAME` always; `$PARAMS` when params vary; `Result<$T>`/`Option<$T>` for common wrapper returns; `$RETURN` for fully heterogeneous returns; `&self`/`&mut self` preserved literal when universal
- Output example: `pub async fn $NAME(&self, $PARAMS) -> Result<$T>`
- Regenerated on every FCA rebuild (ephemeral table); orphans cleaned after each pass

## Design docs

- PRD: yojana task `sutra/57` (not a file on disk)
- ADR-0001: `docs/adr/0001-sketch-mode-conventions-only.md`
- ADR-0002: `docs/adr/0002-ephemeral-durable-partition.md`
- ADR-0003: `docs/adr/0003-layered-adapter-traits.md`
- Domain glossary: `CONTEXT.md`
- Core model PRD: `docs/prd-core-model.md`

## Test locations

- Unit tests: `#[cfg(test)]` modules in `engine.rs`, `lifecycle.rs`, `drift.rs`, `templates.rs`
- Integration tests: `tests/review-test.rs`, `tests/db-test.rs`
- Test DB setup: `Db::open_unchecked("test", dir.path())` with tempdir
