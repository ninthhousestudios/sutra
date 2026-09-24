# Health system architecture map

Quick-reference for agents planning or implementing health/similarity tasks.
Read this first, then do targeted `sutra_outline` / `sutra_symbol` calls on
specific files. Updated after each health-system landing.

Governing contract (sutra/412, approved 2026-09-21; implemented by 413–418 and
416): [health evidence contract](health-evidence-contract.md). Background:
[sutra/411 evidence lifecycle review](reviews/2026-09-21-health-evidence-lifecycle.md).

Last implementation update: 2026-09-22 (sutra/416: scoring consumes the published
run's per-(file, producer) outcomes — no coverage bool, no snapshot mirroring;
partial scores are cap-saturating intervals; snapshots persist a per-file score
basis and upper bound; review separates temporal comparison from on-demand
attribution; trend gates file/aggregate/component deltas on matching basis. See
"Scoring over validated evidence" below. Removed: `WorkspaceFacts`,
`GitAvailability`, `health_coverage` reads/writes, `compute_health_delta`.)

## Module layout

```
src/health/
  mod.rs            — re-exports from findings, git_metrics, and scoring
  findings.rs       — HealthFinding, BiomarkerKind (13 variants + ALL + from_str),
                      HealthSeverity (Advisory, Informational — never Blocking,
                      + from_str), compute_nested_complexity,
                      compute_dead_code_ratio, compute_all_health_findings(db,
                      workspace_root)
  erosion.rs        — Standalone erosion metric (NOT a biomarker, sutra/442):
                      mass/eroded/aggregate/nearest-rank percentiles,
                      samples_by_file (outermost + test exclusion),
                      COGNITIVE_THRESHOLD shared with diff_impact.
  instability.rs    — Component instability (Martin's Ce/(Ca+Ce)).
                      ComponentInstability{ce, ca, instability},
                      compute_component_instability(db). Uses import_edges
                      + component_members_with_line_count to partition
                      directed import edges into efferent/afferent per
                      component. Surfaced in sutra_file_health component scores.
  git_metrics.rs    — git-organizational biomarkers consuming commits +
                      commit_files tables. compute_co_change_scatter,
                      compute_change_entropy, compute_ownership_risk,
                      compute_hidden_coupling. OwnersConfig + load_owners_config
                      for .sutra/owners.toml alias mapping.
  ondemand.rs       — On-demand biomarkers computed at review time via git
                      blame (too expensive for parse-time pipeline).
                      BlameCache (in-memory per-review dedup),
                      compute_ondemand_findings → OnDemandEvidence{findings,
                      outcomes per changed path} (function_hotspot +
                      code_age_volatility; a blame failure is Missing, not a
                      skip), OnDemandEvidence::add_shape_diff (HrrShapeChange
                      Complete/Missing per analyzed/failed path),
                      compute_shape_change_findings.
  scoring.rs        — HealthCategory (5 variants + ALL), caps, weights,
                      severity weights. BiomarkerScope (Persistent/OnDemand/
                      Component; exhaustive) + PERSISTENT_PRODUCERS.
                      EvidencePart{outcomes, findings}; score_file(&[parts]) →
                      FileHealthScore{value: ScoreValue(Measured|Partial{lower,
                      upper}), deductions, categories, missing, unsupported};
                      scenario_score (shared cap/clamp primitive);
                      file_score_basis; SCORING_VERSION; instability_penalty,
                      score_component (NLOC-weighted).
  assess.rs         — PersistentEvidence::load(db, Validity): the current run's
                      outcomes/findings for every indexed file, validity applied,
                      waivers partitioned on captured path/symbol label, per-file
                      basis. score_workspace(db, &evidence) → files + components
                      (component value/bounds + membership basis).
  compare.rs        — BaselineSelector (+resolve), IncomparableReason,
                      SideSummary, temporal_blocker (the one temporal rule, used
                      by trend and review), attribute (on-demand attribution,
                      MarginalEffect Exact|Conditional).

src/parser/
  complexity.rs     — cyclomatic, cognitive, max_nesting_depth (all take
                      tree-sitter Node + src + lang). classify_cognitive
                      shared between cognitive scoring and nesting depth.
                      walk_nesting handles else-if chains as flat (same as
                      cognitive).
  mod.rs            — ExtractedSymbol: cyclomatic, cognitive, max_nesting
                      fields (all Option<u32>)
  rust.rs           — calls max_nesting_depth alongside cyclomatic/cognitive
                      for Function/Method kinds (body node required)
  dart.rs           — same pattern for Dart

src/db/
  graph.rs          — Db methods for git-organizational queries:
                      file_cochange_partners (file_id, partner_count, commit_count),
                      file_commit_sizes(max_width) → (file_id, committed_at, file_count),
                      file_author_commits → (file_id, author, commit_count).
                      Also: cochange_pairs_above_threshold, static_file_edges
                      (both used by hidden_coupling).
  health.rs         — HealthFindingRow, HealthWaiverRow, NestingExceedRow.
                      Db methods: symbols_exceeding_nesting, replace_health_findings
                      (live diagnostic table; scoring reads the run),
                      get_health_findings (optional file_id +
                      biomarker_kind filters), get_health_waivers,
                      create_health_waiver (upsert), delete_health_waiver,
                      get_health_findings_with_waiver_status.
  mod.rs            — TABLE_REGISTRY entries: health_findings (Ephemeral),
                      health_coverage (Ephemeral, vestigial since sutra/416 —
                      no reader or writer), health_waivers (Durable).
                      SnapshotFileRow.completeness/missing_biomarkers/
                      score_upper/score_basis; SnapshotComponentRow.completeness/
                      score_basis; one row writer per snapshot detail table.
                      SymbolRow.max_nesting field. InsertSymbolParams.max_nesting.
  migrations.rs     — 0027 (ephemeral), 0028 (durable), 0071 git_availability
                      (durable ALTER; column unused since sutra/416), 0072
                      health_coverage (ephemeral; unused since 416), 0073/0076
                      snapshot completeness, 0077 snapshot score basis + upper
                      bound + component completeness (ephemeral ALTERs), 0078
                      component member weights + instability penalty

src/similarity/
  hrr.rs            — HrrVec (1024-dim), Complex, FFT-based circular
                      convolution, Rng (deterministic xoshiro256++).
                      Key methods: cosine_similarity, bind/unbind,
                      bundle, permute, to_bytes/from_bytes. Storage format
                      is i8-quantized + 4-byte f32 scale header (~1KB/vec,
                      8× smaller than f64; legacy f64 blobs still decode).
  codebook.rs       — Codebook: content-addressed (sutra/327) — key → HrrVec
                      seeded by FNV-1a hash of the key, per-run memo only,
                      nothing persisted. Deterministic in any encounter order.
  encoder.rs        — encode_subtree(node, source, codebook, embed_idents).
                      embed_idents=false → strip mode (structure only),
                      embed_idents=true → embed mode (structure + names).
  diff.rs           — Semantic diff: four-quadrant classification of
                      text-Δ vs HRR-Δ per function. detect_shape_changes
                      re-parses old source from git, encodes strip vectors,
                      compares against current. Returns Vec<ShapeChange>
                      (file_id/symbol_id resolved). ShapeChangeConfig
                      thresholds (text_delta: 0.15, hrr_delta: 0.15).
                      Integrated into sutra_review as "hrr_shape_changes"
                      output. The SubtleStructural quadrant is converted to a
                      HrrShapeChange HealthFinding by
                      ondemand::compute_shape_change_findings (sutra/402) so it
                      feeds the health delta — diff.rs itself emits no findings.
  duplicates.rs     — find_pattern_families: union-find clustering over
                      strip vectors. Used by sutra_duplicates tool.
  search.rs         — find_similar: cosine-similarity ranked search.
                      SimilarityMatch{symbol_id, score}. Self-exclusion,
                      threshold filtering, limit truncation.
  mod.rs            — compute_hrr_vectors (pipeline entry; per-file encoding
                      parallelized via thread::scope + atomic work queue,
                      SUTRA_HRR_PARALLELISM override), compute_pattern_families.
                      SimilarityMode knob: SUTRA_SIMILARITY_MODE =
                      full|strip-only|off|auto (auto downgrades to strip-only
                      above 200k function symbols; strip-only drops embed
                      vectors, off skips HRR entirely).

src/tools/
  file_health.rs    — MCP tool: scores PersistentEvidence (validity from the
                      demand refresh outcome), builds per-file + per-component
                      JSON. Partial entries: health_score null + score_bounds +
                      missing[{biomarker, reason}]; findings of a non-Complete
                      producer are listed `stale` with deduction 0;
                      `waived_findings` counts waiver-excluded findings.
                      `category_deductions` is known (capped) debt only — the
                      saturated caps behind a partial lower bound appear under
                      `_explain.categories.*.pessimistic_deduction`. Sorted by
                      upper bound. Accepts optional `component` filter (by name).
                      Component scores include instability metrics.
                      handle_ctx gates the component block on
                      components::membership_current (sutra/426): the demand
                      refresh rebuilds file rollups but does NOT re-cluster, so
                      when clustering is stale for the live graph/history/config
                      the component scores (and instability penalty) are computed
                      off a stale grouping — replaced by a `components_unavailable`
                      { reason: "stale_membership" } block, distinct from the
                      current per-file evidence. Every `parse_workspace` repairs
                      it: the full path via post_parse_sequence, and the
                      no-change path via recluster_unchanged after its health
                      refresh (sutra/443). The refresh's history ingest moves
                      newest_commit_at on any new commit, so without that repair
                      the no-change path would checkpoint every component partial.
  trend.rs          — MCP tool: sutra_trend. Comparison mode diffs two
                      snapshots with per-file deltas (improved/degraded/
                      incomparable), per-component deltas, category
                      breakdown. History mode returns per-file score time
                      series. Both carry per-file completeness (sutra/418;
                      see "Trend completeness contract" below).
  similar.rs        — MCP tool: sutra_similar(symbol, mode, limit, threshold).
                      Resolves symbol → HRR vector, linear scan cosine
                      similarity, returns ranked matches with file locations.

src/db/
  similarity.rs     — HrrSymbolRow, SymbolSummary, PatternFamily types.
                      Db methods: function_symbols_for_hrr, replace_hrr_vectors,
                      load_all_strip_vectors, load_hrr_vector (single),
                      load_all_vectors_by_mode, replace_pattern_families,
                      query_pattern_families, symbols_by_ids,
                      function_symbol_count, delete_embed_vectors.
                      (hrr_codebook table dropped by migration 0064.)
  components.rs     — component_members_with_line_count() added for
                      NLOC-weighted component health scoring.

src/pipeline.rs     — post_parse_sequence tail: refresh::publish_run (runs after
                      component discovery and alias sync, before record_snapshot).
                      record_snapshot calls compute_snapshot_health, which
                      validates the current run (refresh::current_run_validity),
                      scores PersistentEvidence via assess::score_workspace and
                      stores lower bound / upper bound / completeness / basis per
                      file and per component, plus the scored run id on the
                      checkpoint (`snapshots.health_run_id`). A NoChanges parse
                      first runs the locked `refresh::refresh`, then copies only
                      the parse-derived aggregates forward and always rescores
                      health (`record_unchanged_snapshot`).
```

## Key types

### HealthFinding (health/findings.rs)
Core finding struct all biomarkers produce. Fields: `file_id: i64`,
`symbol_id: Option<i64>`, `biomarker_kind: BiomarkerKind`,
`severity: HealthSeverity`, `confidence: f64`, `provenance: String`,
`metric_value: f64`, `threshold: f64`, `detail: String`.

### BiomarkerKind (health/findings.rs)
Enum with 13 variants (`ALL` const enumerates them for the scoring contract).
Parse-time file-level: `NestedComplexity`, `CoChangeScatter`, `ChangeEntropy`,
`OwnershipRisk`, `HiddenCoupling`, `ImportCycle`, `DeadCodeRatio`,
`BlastRadiusChurn`. On-demand (review-time): `FunctionHotspot`,
`CodeAgeVolatility` (git blame), `HrrShapeChange` (shape-diff, converted from
ShapeChange). Component-scoped: `ComponentInstability` (computed via
`health/instability.rs`, applied as a component-score deduction via
`instability_penalty`, not a per-file HealthFinding). Unsupported:
`CoverageGradient` (no coverage ingestion exists anywhere in the repo).

`as_str()` returns snake_case DB representation. `from_str()` roundtrips.
`default_severity()` maps tier 1/2 → Advisory, tier 3 + sutra-specific →
Informational. `category()` returns HealthCategory. `default_weight()`
returns repowise-calibrated weight (or moderate/uncalibrated default).
`scope()` (in scoring.rs) is the exhaustive Persistent/OnDemand/Component
classification; `PERSISTENT_PRODUCERS` lists the per-file producers every run
stages an outcome for — see below.

### Scoring over validated evidence (sutra/416)
Findings are positive-only, so their absence proves nothing. Every consumer
therefore scores **outcomes**, not finding presence: the published run stages one
`ProducerOutcome` per (file, persistent producer) — `Complete{n}`,
`Missing(reason)`, or `Unsupported(reason)` — and `assess::PersistentEvidence`
loads them for every indexed file under a caller-supplied `assess::RunVerdict { run, validity }` — a verdict about one specific run:
- `Current` — outcomes as staged, **only if the current pointer still names
  `run`**; if another run was published since, it reads `Stale(InputsChanged)`
  (and no run at all reads `Stale(LegacyUnknown)`). The demand refresh
  establishes it (`DemandOutcome::verdict`: Reused/Published(id) → Current for
  id; InputsChanged, Deferred(..), Failed → Stale(reason)); the snapshot writer
  uses `refresh::current_run_validity` (probe + `evidence::validate`, no
  mutation).
- `Stale(reason)` — every `Complete` and every repository-dependent `Unsupported`
  becomes `Missing(reason)`; only `NoCoverageIngestion` stays unsupported.
- No run at all → `Missing(LegacyUnknown)`; a file the run lacks →
  `Missing(NeverComputed)`.
- A `Complete{n}` whose run did not retain exactly `n` parseable findings for
  that (file, producer) → `Missing(InvalidEvidence)` (never trusted as clean).

`score_file(&[EvidencePart])`: a finding counts only when **its own part**
recorded its producer `Complete` (a stale or unauthorized finding is never known
debt). Per category, the optimistic deduction is capped known debt; if any
producer in the category is `Missing`, the pessimistic deduction is the full
cap. `Measured(score)` only when nothing is missing, otherwise
`Partial{lower, upper}` (global [1, 10] clamp applied to each). One missing
finding's weight is not a proved worst case, so the old one-weight worst-casing
is gone. `Unsupported` producers are excluded and surfaced.

Consequences worth knowing: a file with no in-window commits (sutra/423) has its
git producers `Missing(NoHistory)`, so organizational (3.5), structural (2.5, via
blast_radius_churn) and coupling (2.0, via hidden_coupling) all saturate in the
lower bound — such files report bounds, not a point score. That is the approved
contract decision ("missing per-file history stays partial"), not a bug.

**Score basis.** `scoring::file_score_basis` digests SCORING_VERSION,
HEALTH_ANALYSIS_VERSION, every severity's weight, every persistent producer's
default-severity weight, weight,
category and cap, the file's applicability (which producers are Unsupported and
why) and the waiver policy for the path (sorted `(biomarker, symbol)` of its
waivers). A `Missing` producer is still applicable, so completeness transitions do
not change the basis. Input generations and history windows are deliberately not
in it. Component basis (`assess::score_members`): aggregation version,
instability-penalty form, and the sorted member (path, file basis) set — so a
membership change is a basis change.

**Waivers** are partitioned once, in `PersistentEvidence::load`, over the run's
captured path + symbol label (`waivers::partition`), and review applies the same
policy to on-demand findings. Waived findings stay visible (`FileEvidence.waived`).

Snapshots persist completeness + `missing_biomarkers` + `score_upper` +
`score_basis` per file (`health_snapshot_files`; `score` is the conservative lower
bound; `category_scores` holds the pessimistic per-category deductions), and
completeness + basis per component (lower bound only — component upper bounds
are not persisted), plus each component's member weights
(`health_snapshot_component_members`) and instability penalty (sutra/436). The snapshot-level `health_score` is the mean of file lower
bounds and is only a measured aggregate when `aggregate_comparison.measured`.

**Trend completeness contract (sutra/418).** `SnapshotFileRow.completeness`
is `Complete | Partial | Unknown`, stored as `partial` + `completeness_recorded`
(migration 0076). `Unknown` = the row never recorded completeness: pre-0073 rows
and every row the production writer (`insert_snapshot_atomic`) wrote before
sutra/418, which silently dropped both columns. A defaulted `partial = 0` is
never read as complete and is not backfilled. Both snapshot inserts share one
row writer (`insert_snapshot_file_rows`); `missing_biomarkers` is stored sorted
(the scorer's order is unstable) and compared as a set.

Additive output fields (existing fields keep their meaning):
- Every completeness object is `{completeness: "complete"|"partial"|"unknown",
  partial: bool | null, missing_biomarkers: [..]}`; `partial` is `null` for
  Unknown.
- History entries carry those three keys flat, next to the score, which goes
  through the same stored-score serializer as review baselines
  (`file_health::stored_score_json`, sutra/438): complete under a recorded
  basis → numeric `health_score`; partial under a recorded basis →
  `health_score: null` + `score_bounds {lower, upper}` + `partial: true`;
  anything else (Unknown completeness, or no `score_basis` — pre-416 rules) →
  `health_score: null` + `legacy_score`. A malformed stored `category_scores`
  is an error, not an empty object — in history and in comparison category
  totals alike (`trend::parse_category_scores`, sutra/441).
- Comparison `files.improved`/`files.degraded` entries add `from_completeness`
  and `to_completeness`. They now contain **only** complete→complete pairs.
- Comparison `files.incomparable`: `{path, from, to, from_completeness,
  to_completeness, completeness_changed, reason}`, no `delta`. `reason` is
  `new_file` | `removed_file` | `unknown_completeness` | `partial`. A pair is
  listed when its score or its completeness changed — so equal-score
  completeness transitions stay visible.
- Top-level `completeness: {from, to}` counts files per status on each side.
- Top-level `aggregate_comparison: {measured, reason}`. `deltas.health_score`
  and every `categories.*.delta` are numbers only when `measured`; otherwise
  `null` (the `from`/`to` observations stay). `reason`: `no_file_evidence`
  (either side has no per-file rows) | `incomplete_evidence` (any file on either
  side is partial/unknown) | `population_changed` (file sets differ). Parse
  counters in `deltas` (`files_parsed`, `total_complexity`, ...) are exact and
  always reported.
- **Basis (sutra/416).** Pairs also need matching non-null `score_basis`.
  Additional `files.incomparable` reasons: `unknown_basis` (a side predates 0077)
  and `score_basis_changed`; entries carry `basis_changed`, and a pair is listed
  at equal score/completeness when only its basis moved. `aggregate_comparison`
  adds the same two reasons (checked after `population_changed`).
- Every file/component rule goes through `compare::temporal_blocker`, the same
  rule review uses.
- **Components (sutra/416).** Entries gain `measured`, `reason`,
  `from_completeness`/`to_completeness`; `delta` is null unless measured. New
  components have `from: null` (`reason: new_component`, no 10.0 fallback);
  removed components are listed (`removed_component`, `to: null`). Other reasons:
  `unknown_completeness`, `partial`, `unknown_basis`, `score_basis_changed`
  (membership or member basis moved). Order: measured deltas worst-first, then
  incomparable entries by name.
- **Component weights (sutra/436).** The component basis covers membership and
  member bases but deliberately not member weights (line counts) — putting them
  in would make nearly every edited component incomparable. So a matching basis
  does not make the raw score difference a quality change: a comment-only edit
  moves a member's weight and with it the weighted mean. Component entries
  replace `delta` with `measured_delta` + `weight_shift`:
  `measured_delta = component_score(current member scores @ baseline weights,
  current penalty) − component_score(baseline scores @ baseline weights,
  baseline penalty)` (`scoring::component_score`, the scorer's own rule), and
  `weight_shift = (to − from) − measured_delta` — line-count/mix movement, never
  a measured improvement/degradation. An instability-penalty change counts as
  measured (both penalties known). Both are null unless `measured`. Extra
  reasons: `unknown_weights` (a side predates 0078 — never measured at a
  defaulted weight), `unknown_instability` (a side's penalty unknown),
  `inconsistent_members` (recorded members disagree, or a member lacks a
  complete file row, despite the matching basis). Measured entries sort by
  `measured_delta`.
- History entries add `score_bounds {lower, upper}` for partial rows written
  since 0077.
- NoChanges parses no longer copy health forward (sutra/416 review H1/H2): they
  refresh health under the held lock (republishing after a crossed midnight,
  HEAD move or config edit), copy only parse-derived aggregates, and rescore
  every health row from the validated run. This also covers sutra/429 for the
  persistent run.
- Provenance: snapshot JSON carries `health_run_id`; comparison adds top-level
  `input_changes` — the input axes that moved between the two checkpoints' runs
  (`compare::input_changes`: reindexed, graph_rules, graph, indexed_paths,
  analysis_version, history_head, history_window, history_day, history_state,
  owners_config, rollups), or `null` when a side is legacy.
- Incomparable file entries add `from_bounds` / `to_bounds` (`{lower, upper}` for
  partial rows written since 0077, else `null`); `from`/`to` hold the lower bound.

Behaviour changes that are not additive, per health-evidence-contract.md
§ Comparison: new files no longer compare against a fallback 10.0 (they were
listed as improved/degraded), and removed files moved from `degraded`
(`delta: "removed"`) to `incomparable`; `deltas.health_score` and category
deltas can be `null`; component `delta` can be `null` and removed components now
appear (sutra/416). Component `delta` is replaced by `measured_delta` +
`weight_shift` (sutra/436), and components compared against a pre-0078 snapshot
become `unknown_weights` instead of measured.

### HealthSeverity (health/findings.rs)
Enum: `Advisory`, `Informational`. Health never blocks — that's the
constraint system's job. If a user wants a health threshold to block,
they write a constraint rule.

### HealthFindingRow (db/health.rs)
DB row type for `health_findings` table. Same fields as HealthFinding
but with `id: i64` and string representations for kind/severity.

### HealthWaiverRow (db/health.rs)
DB row for `health_waivers` table. Fields: `id`, `biomarker_kind`,
`file_path`, `symbol_qualified_name: Option`, `rationale`, `waived_by`,
`created_at`, `updated_at`. Mirrors ConstraintWaiverRow shape.

## Database tables

| Table | Partition | Migration | Purpose |
|---|---|---|---|
| health_findings | Ephemeral | 0027 | Computed findings, rebuilt each parse |
| health_waivers | Durable | 0028 | User-authored waivers, survive reindex |
| symbols (max_nesting col) | Ephemeral | 0027 | ALTER TABLE adds max_nesting INTEGER |
| health_snapshot_files | Ephemeral | 0033, 0073, 0076, 0077 | Per-file score (lower bound), completeness, missing producers, upper bound, score basis |
| health_snapshot_components | Ephemeral | 0033, 0077, 0078 | Per-component aggregated scores (lower bound), completeness, membership basis, `weights_recorded`, instability penalty |
| health_snapshot_component_members | Ephemeral | 0078 | Per-snapshot (component, member path, weight = line count); a file may be in several components |
| index_meta (index_epoch col) | Durable | 0074 | ALTER adds index_epoch TEXT; minted lazily, NULLed+reminted on reindex (sutra/414) |
| health_runs | Ephemeral | 0075 | Immutable health evidence runs (FK-free JSON blobs: input_stamp, outcomes, findings). Pruned to the current run plus runs referenced by retained snapshots, on publish and on snapshot prune (sutra/432) |
| health_current | Ephemeral | 0075 | Single-row atomic pointer to the current health_runs.run_id |

Dropped in 0045: convention_snapshots (previously stored FCA conformance
and HRR coherence metrics for drift trending).

### Health evidence storage (sutra/414)

The health-evidence contract (sutra/412) validity/storage layer:
- `src/health/evidence.rs` — owned value types (`Digest`, `IndexEpoch`,
  `Generation`(i64), `RunId`(i64), repository/history/owners/graph stamps,
  `InputStamp`, `ProducerOutcome`, `StoredRun`/`PublishRun`) + the conservative
  `validate(recorded, observed) -> Validity`. Any diverging input axis is
  `Stale`; an observed probe failure is `Failed(..)` not a clean/empty
  observation. Serializes biomarkers through the canonical snake_case
  vocabulary. Scoring/comparison (416) and the locked-refresh staging trait
  (415) are intentionally NOT here.
- `src/db/health_evidence.rs` — `index_epoch`/`ensure_index_epoch`,
  `publish_health_run` (atomic run + pointer; aborts `Ok(None)` if
  `data_generation` moved since the inputs were observed — the mixed-generation
  guard), `load_current_health_run` / `load_health_run` (current or snapshot-referenced runs only).
  `get_derived_complete_generation` is the reader partner to
  `set_derived_complete`; health publication never advances it.
- Legacy indexes have zero runs → readers return `None` (LegacyUnknown); no
  backfill from content hash / HEAD / snapshot completeness.
- Tests: `tests/health_evidence_test.rs` (migration/round-trip/invalidation/
  stale-generation/reindex) + unit tests in `evidence.rs`.
- Remaining slices consume this: 415 (locked refresh, input probing, consumer
  adapters), 416 (scoring/comparison/attribution), 417 (fallible repo probe),
  418 (snapshot/trend completeness).

Migration 0027 is `ephemeral_only: true` — on reindex, symbols table is
dropped and recreated by 0001, then 0027 re-runs the ALTER TABLE.

## Waiver mechanism

Parallel to constraint waivers, not shared tables:
- Identity key: `(biomarker_kind, file_path, COALESCE(symbol_qualified_name, ''))`
- Upsert on conflict (updates rationale, waived_by, updated_at)
- Matching in `get_health_findings_with_waiver_status`: joins findings to
  waivers via file path lookup, returns `Vec<(HealthFindingRow, bool)>`
- Waived findings are visible but flagged — callers exclude from scoring

No MCP tool for health waivers yet. Internal API only.

## Pipeline integration

```
parse_workspace / parse_changed_files
  └── per-file: parse_single_file
        └── ExtractedSymbol.max_nesting set by language adapter
        └── insert_symbols_dfs writes max_nesting to symbols table
  └── post_parse_sequence
        └── ... ref resolution, graph rollups, git co-change, components ...
        └── compute_all_health_findings(db, workspace_root)
              └── compute_nested_complexity: query symbols WHERE max_nesting > 4
              └── compute_co_change_scatter: file_cochange_partners query
              └── compute_change_entropy: file_commit_sizes + decay weighting
              └── compute_ownership_risk: file_author_commits + owners.toml aliases
              └── compute_hidden_coupling: cochange_pairs - static_file_edges
              └── compute_import_cycle_membership: import_edges → Tarjan SCC
              └── compute_dead_code_ratio: dead_code_ratio_by_file query
              └── compute_blast_radius_churn: files.blast_radius + per-file churn
        └── replace_health_findings(findings) — DELETE + INSERT all
  └── record_snapshot
        └── compute_snapshot_health: scores all files via scoring::score_file
              (1.0–10.0 scale, category-capped), aggregates to components
              via scoring::score_component (NLOC-weighted)
        └── insert_snapshot (aggregate metrics + f64 health_score)
        └── insert_snapshot_files (per-file scores + category_scores JSON)
        └── insert_snapshot_components (per-component NLOC-weighted scores)
```

Incrementality: `replace_health_findings` does a full replace each parse.
This is fine at current scale. Future optimization: scope to changed files
using `file_ids_needing_resolution`.

## nested_complexity biomarker

- Threshold: 4 (hardcoded const `NESTING_THRESHOLD`)
- Severity: Advisory
- Confidence: 1.0 (deterministic)
- Provenance: "computed"
- Metric: max_nesting_depth of the function body
- Nesting classification: reuses `classify_cognitive` from complexity.rs
  - Rust: if, while, for, loop increment nesting; match does not; closures do
  - Dart: if, while, for, do increment; switch does not; function expressions do
  - Else-if chains are flat (no extra nesting per chained if)

## erosion (standalone, not a biomarker)

`health/erosion.rs` (sutra/442; design and rejected alternatives in sutra/403's
decisions). Measures how concentrated complexity mass is, adapted from trellis.
It produces no findings, no producer outcome, no deduction and no score-basis
input — health scores are unaffected.

- `mass = cognitive × sqrt(end_line − start_line + 1)`; eroded when
  `cognitive >= COGNITIVE_THRESHOLD` (15). The constant is shared with
  `diff_impact`'s risk gate.
- Only outermost complexity-bearing symbols count (no ancestor with a
  non-null cognitive): the complexity walkers descend into nested functions,
  so a nested JS/TS function is already inside its parent's score.
- Test code is excluded: files matching `components::is_test_file`, and
  symbols (or ancestors) with `FLAG_TEST` (0x01, all parsers) or 0x02
  (`cfg(test)`/test path in Rust and Dart only — TypeScript uses 0x02 for
  `override`). A top-level `#[cfg(test)] fn` outside a test module is not
  flagged by the Rust parser, so it still counts.
- Scopes (file, component, workspace) always SUM function masses. Workspace
  sums over files, not components (multi-membership). Empty scope → null
  share and percentiles. Rank/trend by absolute `eroded_mass`; `eroded_share`
  is descriptive only (non-monotone, bimodal at component scope).
- Persistence: `snapshots.eroded_mass/total_mass/erosion_version`, NULLABLE
  (migration 0079): pre-metric checkpoints read null, never 0. Computed in
  `compute_parse_aggregates`; a NoChanges parse copies it forward only when
  the previous checkpoint has the current `EROSION_VERSION`, otherwise
  recomputes.
- Surfacing: `sutra_file_health` per-file and per-component `erosion` blocks
  (the component block disappears with the rest of `components` when
  membership is stale); `sutra_trend` `deltas.eroded_mass/total_mass`, null
  unless both checkpoints carry the same non-null version.

## git-organizational biomarkers (git_metrics.rs)

All consume `commits` + `commit_files` tables populated by pipeline.
No separate `git log` subprocess. File-level (symbol_id: None).

### co_change_scatter (weight 1.80, Advisory)
- Fires when: distinct co-change partners >= 8 AND commit count >= 3
- DB query: `file_cochange_partners()` — self-join on commit_files
- Metric: partner count. Threshold: 8.

### change_entropy (weight 1.51, Advisory)
- Hassan's History Complexity Metric (ICSE 2009)
- Per commit touching file: contribution = (1/F) × log2(F) × decay
- Decay: half-life 180 days, reference time = newest commit in DB
- Commits wider than 30 files excluded (noise filter)
- Single-file commits contribute zero (log2(1) = 0)
- Threshold: 3.0 (P90 across manas + redox-kernel corpora)

### ownership_risk (weight 1.38, Advisory)
- Fires when: top owner share < 40% OR 3+ minor contributors (< 5% each)
- DB query: `file_author_commits()` — GROUP BY file_id, author
- `.sutra/owners.toml` alias mapping: `[aliases]` section maps
  agent emails to canonical human emails. Without file, each author
  is treated as distinct (conservative default).
- Metric: top owner share (if top trigger) or minor count (if minor trigger)

### hidden_coupling (weight 1.00, escalating severity)
- Reuses `cochange_pairs_above_threshold(0.50)` minus `static_file_edges()`
- 50-65% Jaccard → Informational, >= 65% → Advisory
- Emits two findings per pair (one per file)
- Static edges: resolved refs + imports between files

## On-demand biomarkers (health/ondemand.rs)

Computed at review time via `git blame --porcelain` — too expensive for
parse-time pipeline. BlameCache deduplicates blame calls per file within
a single review invocation.

### function_hotspot (weight 1.16, Advisory)
- Per-function distinct commit count from blame line ranges
- Fires when: distinct_commits >= p80 across changed files' functions
  (floor at 5) AND (cyclomatic >= 10 OR max_nesting >= 3)
- Symbol-level (symbol_id set)
- Provenance: `on-demand:blame`

### code_age_volatility (weight 1.10, Informational)
- Median line age per function from blame timestamps
- Fires when: median_line_age >= 365d AND distinct_commits_in_last_30d >= 2
- Symbol-level
- Provenance: `on-demand:blame`

### Review health (sutra/416)
`tools::review::review_health` produces `health_delta`:
`{baseline_snapshot_id, persistent_validity, temporal_incomparable, files[]}`.
- Baseline: `compare::BaselineSelector` — review pins `Pinned(id)` before
  `tool_context` can record a newer checkpoint; `Pinned(None)` stays missing
  (`temporal_incomparable: "missing_baseline"`), never healed (sutra/424 F5).
- **Temporal** (per changed file): baseline snapshot row vs the current run's
  observation (`PersistentEvidence` under the refresh outcome's validity), via
  `compare::temporal_blocker`. `{measured: true, delta, from, to}` only for
  complete + matching basis; otherwise `{measured: false, reason, from, to}`.
  Pre-0077 baseline rows show `legacy_score`, not a bound. On-demand findings are
  never part of either side — a parse-time checkpoint has no blame/shape
  evidence, so their "history" cannot be invented.
- **on_demand** (per changed file with on-demand outcomes or findings):
  `compare::attribute(persistent, ondemand)` — `without`/`with` score values,
  `effect` `{kind: exact, value}` or `{kind: conditional, lower, upper}`, per
  finding `raw_deduction`/`scaled_deduction` (caps shared with persistent
  findings, so a saturated cap gives an exact 0 effect despite a real finding),
  and `missing` on-demand producers. Conditional bounds: upper assumes missing
  persistent debt saturates its categories and missing on-demand producers found
  nothing; lower the reverse (both via `scoring::scenario_score`).
- A file is listed when its measured delta moved, its incomparable pair changed
  (score, completeness or basis), or it has on-demand findings/missing evidence.
- `health_findings` (display) lists on-demand findings with a `waived` flag.
- Provenance: `baseline_run_id`, `current_run_id` and `input_changes` (same
  tokens as trend) explain what moved besides the diff.
- A storage error while pinning the baseline fails the tool rather than reading
  as `missing_baseline`.
- Any failure (blame/storage/scoring) surfaces as `health_delta_error`; nothing
  is swallowed with `.ok()`.

## PRD and arc context

- PRD: yojana task `sutra/83` (health metrics + similarity system)
- Arc: 5e (health + similarity), implement phase
- Repowise survey: `docs/survey-repowise-health.md` (empirical foundation)
- HRR spike: branch `spike/hdc-ast-encoding`

### Biomarker tiers (from PRD)

| Tier | Severity | Biomarkers | Weight source |
|---|---|---|---|
| 1 | Advisory | co_change_scatter, change_entropy, ownership_risk, function_hotspot | repowise ≥1.3 |
| 2 | Advisory | nested_complexity, hidden_coupling, blast_radius_churn | repowise moderate |
| 3 | Informational | dead_code_ratio, code_age_volatility, coverage_gradient | repowise weak |
| Sutra | Informational | component_instability, hrr_shape_change, import_cycle | uncalibrated |

### Producer status (verified sutra/402)

| Biomarker | Produced by | Notes |
|---|---|---|
| nested_complexity | findings.rs (parse) | |
| co_change_scatter / change_entropy / ownership_risk / hidden_coupling | git_metrics.rs (parse) | git-gated: confirmed non-repository → Unsupported (excluded); no usable per-file history → Missing(NoHistory); probe/ingestion failure → Missing(Failed) — both saturate their category in the lower bound. Shallow clone → history is `Unknown(HistoryIncomplete)` → Missing(Failed) / partial, never Complete: a truncated object graph cannot positively establish window completeness (sutra/427). A shallow clone is always re-ingested (never reuses), so deepening at unchanged HEAD is picked up; `git::history_boundaries` also fingerprints the actual `.git/shallow` boundary commit set, not just the is-shallow boolean |
| import_cycle | findings.rs (parse) | |
| dead_code_ratio | findings.rs compute_dead_code_ratio (parse) | provisional threshold 0.15 (uncalibrated) |
| blast_radius_churn | git_metrics.rs compute_blast_radius_churn (parse) | provisional: blast_radius ≥ 10 AND churn ≥ 5 (uncalibrated) |
| function_hotspot / code_age_volatility | ondemand.rs (review, blame) | |
| hrr_shape_change | ondemand.rs compute_shape_change_findings (review) | from SubtleStructural ShapeChange |
| component_instability | scoring.rs instability_penalty (component) | not a per-file finding |
| coverage_gradient | none | Unsupported — no coverage ingestion in repo |

### Health scoring (sutra/85, implemented)

`health/scoring.rs`: base 10.0, deductions per finding
(`severity.weight() × biomarker.default_weight()`), capped per category:

| Category | Cap | Biomarkers |
|---|---|---|
| organizational | -3.5 | co_change_scatter, change_entropy, ownership_risk |
| structural | -2.5 | nested_complexity, function_hotspot, blast_radius_churn |
| coupling | -2.0 | hidden_coupling, component_instability, import_cycle |
| freshness | -1.5 | code_age_volatility, hrr_shape_change |
| coverage | -2.0 | dead_code_ratio, coverage_gradient |

Severity weights: Advisory = 1.0, Informational = 0.5.
Proportional scaling within category when sum exceeds cap.
Component scores: NLOC-weighted average of member file scores, minus
`instability_penalty` (Informational × ComponentInstability weight × I, capped
at the coupling cap), computed on member lower and upper bounds; measured only
when every member is measured, membership is current
(`components::membership_current`; the snapshot writer passes it, file_health's
ctx path replaces stale components with `components_unavailable`) and
instability computed. An instability failure is not fatal: the penalty becomes
unknown and the lower bound drops by the maximum penalty. Final clamp [1.0, 10.0].

Missing analysis (sutra/416): see "Scoring over validated evidence" — a missing
producer saturates its category cap in the lower bound; the score is an interval,
not a worst-cased point value. `Unsupported` dimensions (coverage_gradient always;
git biomarkers only on a confirmed non-repository) are excluded and surfaced.

Calibrated biomarker weights (from repowise T0-protocol corpus):
co_change_scatter 1.80, change_entropy 1.51, ownership_risk 1.38,
nested_complexity 1.34, function_hotspot 1.16, code_age_volatility 1.10.
Non-repowise defaults: hidden_coupling 1.00, blast_radius_churn 1.00,
dead_code_ratio 0.80, coverage_gradient 0.80. Uncalibrated:
component_instability 0.50, hrr_shape_change 0.50, import_cycle 0.50.

The `file_health` MCP tool returns findings + derived scores (1.0–10.0
scale). The pipeline snapshot system also uses `scoring::score_file` —
legacy `compute_file_scores` (0–100 scale) has been removed.

### Remaining arc tasks

| Task | Title | Status | Key concern |
|---|---|---|---|
| sutra/84 | health finding model + first biomarker | done | this doc |
| sutra/85 | health scoring with category capping | done | scoring.rs + tool rewrite |
| sutra/86 | git-organizational biomarkers | done | git_metrics.rs, db/graph.rs queries |
| sutra/87 | review-1: health foundation | done | review gate |
| sutra/88 | HRR encoder | done | similarity/hrr.rs, encoder.rs, codebook.rs |
| sutra/89 | structural similarity search | done | similarity/search.rs, tools/similar.rs |
| sutra/90 | pattern families + duplicates | done | similarity/duplicates.rs, tools/duplicates.rs |
| sutra/91 | health snapshots + per-file history | done | pipeline.rs, trend.rs, db/mod.rs |
| sutra/93 | semantic diff for review | needs-review | similarity/diff.rs, review.rs |
| sutra/94 | review integration — health delta + on-demand biomarkers | needs-review | health/ondemand.rs, git.rs, review.rs |
| sutra/95 | convention drift detection | removed | dropped in sutra/232 (zero usage) |
| sutra/96 | orient + health MCP tools | done (orient later deleted, sutra/312) | health/instability.rs, ~~tools/orient.rs~~, tools/file_health.rs |

## Test locations

- Unit tests: `#[cfg(test)]` in `src/parser/complexity.rs` (5 nesting depth tests)
- Unit tests: `#[cfg(test)]` in `src/similarity/search.rs` (5 search tests)
- Unit tests: `#[cfg(test)]` in `src/graph.rs` (7 SCC tests)
- Unit tests: `src/health/{scoring,assess,compare}.rs` (bounds, validity,
  basis, temporal rule, attribution incl. saturated caps / conditional bounds)
- Real-path: `tests/health-refresh-test.rs` § sutra/416 (full parse → comment
  edit → incremental → refresh → review; stale refresh; debt removal; legacy
  baseline; blame hotspot attribution; per-file history; waiver basis in trend)
- Test support: `tests/support/health_run.rs::publish_run_with` publishes seeded
  findings as a run (scoring reads only published runs)
- Integration tests: `tests/health-test.rs` (model, threshold,
  DB round-trip, waiver CRUD, waiver exclusion, scoring, git-organizational
  biomarkers: scatter, entropy, ownership, coupling, alias merging,
  snapshot per-file/per-component storage, file health history,
  trend comparison with file deltas, trend history mode, blame parsing,
  HealthFinding::to_row, health delta: degradation, improvement,
  no-snapshot fallback, on-demand finding attribution,
  component instability: basic, isolated, fully-efferent,
  file health: component filter, component instability in scores, partial
  bounds; validated evidence (stale run, legacy, never-computed, waivers/basis);
  trend component basis gating, component deltas at baseline weights,
  import cycle: cyclic fires, acyclic absent, DB roundtrip)
- Integration tests: `tests/similarity_test.rs` (12 tests — HRR vectors,
  strip/embed modes, determinism, discrimination, pattern families,
  similarity search: strip mode, embed vs strip, self-exclusion, diagnostics)
- Test DB setup: `Db::open_unchecked("test", dir.path())` with tempdir
- Seed helpers: `seed_fn(db, file_id, qn, sn, max_nesting)`,
  `seed_commits(db, commits, pairs)` in health-test.rs
