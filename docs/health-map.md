# Health system architecture map

Quick-reference for agents planning or implementing health/similarity tasks.
Read this first, then do targeted `sutra_outline` / `sutra_symbol` calls on
specific files. Updated after each health-system landing.

Approved replacement contract (sutra/412, 2026-09-21; implementation in 413–418):
[health evidence contract](health-evidence-contract.md). It includes the checked
type skeleton and downstream boundaries; it is not implemented behavior.

Review correction: 2026-09-21 — [sutra/411 evidence lifecycle review](reviews/2026-09-21-health-evidence-lifecycle.md)
confirms incremental parsing deletes findings/history and falsifies the
snapshot-mirroring rationale below. The recorded two-axis fix also needs revision:
hidden coupling depends on the current graph, and graph validity is not per-file
content validity. Session-start reparse removal is not a requirement.

Last implementation update: 2026-09-19 (sutra/408: made the "missing analysis is never zero
debt" contract actually fire — per-file health_coverage stamp so incrementally
reparsed / finding-free files are worst-cased not floored, GitAvailability axis
separating NotARepo from NoHistory, snapshot partial/missing_biomarkers columns.
Prior: sutra/402 landed the contract — producers, ComponentInstability
deduction, CoverageGradient unsupported)

## Module layout

```
src/health/
  mod.rs            — re-exports from findings, git_metrics, and scoring
  findings.rs       — HealthFinding, BiomarkerKind (13 variants + ALL + from_str),
                      HealthSeverity (Advisory, Informational — never Blocking,
                      + from_str), compute_nested_complexity,
                      compute_dead_code_ratio, compute_all_health_findings(db,
                      workspace_root)
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
                      compute_ondemand_findings (function_hotspot +
                      code_age_volatility), compute_shape_change_findings
                      (SubtleStructural ShapeChange → HrrShapeChange finding),
                      compute_health_delta (per-file score comparison vs latest
                      snapshot with attribution).
                      FunctionBlameStats, HealthDelta, HealthDeltaEntry.
  scoring.rs        — HealthCategory (5 variants), category caps, biomarker
                      weights (repowise calibrated), severity weights,
                      WorkspaceFacts{git: GitAvailability} + detect (reads
                      persisted index_meta.git_availability, falls back to
                      commit-count), GitAvailability
                      (Available/NoHistory/NotARepo), BiomarkerSupport
                      (Scored/Unsupported/Unwired),
                      BiomarkerKind::file_scoring_support (single source of
                      truth), score_file(findings, facts, covered) — category
                      capping + proportional scaling + worst-cases Unwired
                      biomarkers AND (when !covered) Scored ones
                      (MissingDeduction, FileHealthScore.missing/partial()),
                      instability_penalty, score_component (NLOC-weighted),
                      score_workspace(db) (scores every file via
                      health_coverage_map; instability always computed +
                      applied), FileHealthScore, FindingDeduction

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
                      (also stamps health_coverage for all files),
                      health_coverage_map, get_health_findings (optional file_id +
                      biomarker_kind filters), get_health_waivers,
                      create_health_waiver (upsert), delete_health_waiver,
                      get_health_findings_with_waiver_status.
  mod.rs            — TABLE_REGISTRY entries: health_findings (Ephemeral),
                      health_coverage (Ephemeral), health_waivers (Durable).
                      git_availability / set_git_availability (index_meta).
                      SnapshotFileRow.partial + .missing_biomarkers.
                      SymbolRow.max_nesting field. InsertSymbolParams.max_nesting.
  migrations.rs     — 0027 (ephemeral), 0028 (durable), 0071 git_availability
                      (durable ALTER on index_meta), 0072 health_coverage
                      (ephemeral), 0073 snapshot completeness (ephemeral ALTER)

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
  file_health.rs    — MCP tool: queries findings with waiver status, scores
                      via scoring::score_file, builds per-file + per-component
                      JSON. Accepts optional `component` filter (by name).
                      Component scores include instability metrics.
                      handle_ctx gates the component block on
                      components::membership_current (sutra/426): the demand
                      refresh rebuilds file rollups but does NOT re-cluster, so
                      when clustering is stale for the live graph/history/config
                      the component scores (and instability penalty) are computed
                      off a stale grouping — replaced by a `components_unavailable`
                      { reason: "stale_membership" } block, distinct from the
                      current per-file evidence. Full parse re-clusters and repairs.
  trend.rs          — MCP tool: sutra_trend. Comparison mode diffs two
                      snapshots with per-file deltas (improved/degraded),
                      per-component deltas, category breakdown. History
                      mode returns per-file score time series.
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

src/pipeline.rs     — post_parse_sequence tail: compute_all_health_findings
                      + replace_health_findings (runs after component discovery
                      and alias sync, before record_snapshot).
                      record_snapshot calls compute_snapshot_health which
                      scores all files via scoring::score_file and aggregates
                      to components, storing per-file and per-component data.
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
`file_scoring_support(facts)` (in scoring.rs) is the single source of truth
for the "missing analysis is never zero debt" contract — see below.

### The zero-debt contract (scoring.rs, sutra/402 + sutra/408)
Findings are positive-only (emitted only on a problem), so absence used to be
indistinguishable between "clean", "not applicable", and "check never ran" —
all scored as zero debt. `file_scoring_support(&WorkspaceFacts)` classifies
each biomarker for file-level parse-time scoring:
- `Scored` — producer wired + data source present. Worst-cased anyway when the
  file's analysis is not current (see coverage below).
- `Unsupported(reason)` — data source **structurally** absent (not a git repo;
  no coverage ingestion). Excluded from scoring, surfaced as
  `unsupported_biomarkers`.
- `Unwired` — should be scored but has no data this run: no producer, or a git
  data source that was unavailable (empty commit window / `git log` failure).
  `score_file` worst-cases it at full weight (within category caps) and sets
  `FileHealthScore.missing` / `partial()`; `file_health` surfaces per-file
  `partial` + `missing_biomarkers`.
- `None` — scored elsewhere (review-time on-demand or component-scoped).
The match is exhaustive, so a new variant forces a decision.

**Per-file coverage (sutra/408).** Workspace-scope "producer wired" is not
per-file "producer ran for THIS file's current content." The `health_coverage`
table records the `content_hash` each file's findings were computed for,
stamped atomically inside `replace_health_findings` (which runs only after a
full `compute_all_health_findings` over every file). `score_workspace` scores
**every** indexed file — not just files with findings — and passes
`covered = coverage[file_id] == file.content_hash` to `score_file`. When
`!covered` (an incrementally-reparsed file whose findings were never recomputed,
or a newly-added file with none), the stale present findings are ignored and
every `Scored`/`Unwired` file-scored biomarker is worst-cased. This is what
makes the contract fire on the live query path: `parse_incremental` never
recomputes health findings, so without coverage a drifted file would float back
to a clean 10.0.

**Per-axis coverage (sutra/409).** `covered` in `score_file` governs only the
*file-scored* biomarkers (`file_scoring_support` is `Some`). A present finding
whose biomarker is review-time on-demand or component-scoped (`None`) is trusted
regardless of `covered`, because the caller recomputes it fresh every run. This
lets one `score_file` call worst-case a stale structural axis while still
crediting fresh on-demand debt in the same (cap-sharing) score — required by the
delta path below. `score_workspace`/`file_health` pass uniform per-file coverage,
so their behavior is unchanged.

**Git availability axis (sutra/408).** `WorkspaceFacts.git: GitAvailability`
(`Available` | `NoHistory` | `NotARepo`) replaces the old `has_git` bool and is
persisted on `index_meta.git_availability` at parse time (where the git outcome
is observed; `detect` only has the `Db`). Only `NotARepo` (true structural
absence) → `Unsupported`; `NoHistory` (empty window or a transient `git log`
failure) → `Unwired` (worst-cased). A `git log` failure no longer clears
`commit_files`, so health cannot improve by losing evidence. `detect` falls back
to the `commit_file_count` heuristic on indexes predating the column.

Snapshots persist `partial` + `missing_biomarkers` per file
(`health_snapshot_files`), so `trend` can tell partial analysis from real
degradation.

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
| health_snapshot_files | Ephemeral | 0033 | Per-file health scores at each snapshot |
| health_snapshot_components | Ephemeral | 0033 | Per-component aggregated scores at each snapshot |
| index_meta (index_epoch col) | Durable | 0074 | ALTER adds index_epoch TEXT; minted lazily, NULLed+reminted on reindex (sutra/414) |
| health_runs | Ephemeral | 0075 | Immutable, insert-only health evidence runs (FK-free JSON blobs: input_stamp, outcomes, findings) |
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
  guard), `load_current_health_run` / `load_health_run` (retained diagnostics).
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

### Health delta (review integration)
- `compute_health_delta(db, changed_paths, ondemand_findings, baseline: BaselineSelector)`
  compares current per-file scores (stored findings + on-demand) against a
  baseline snapshot. Returns `HealthDeltaOutcome` — `Measured(HealthDelta)` or
  `Incomparable(IncomparableReason)`.
- `BaselineSelector` (sutra/424 F5): `Latest` falls back to the newest
  checkpoint at compute time (non-review callers, pre-pinning behaviour);
  `Pinned(Some(id))` uses the caller's pre-request baseline; `Pinned(None)` is a
  genuinely-missing baseline → `Incomparable(MissingBaseline)`, so review does
  not compare against a snapshot healed into being during its own request.
- Review gates the delta on refresh validity (sutra/424 F3): `sutra_review`
  captures the `DemandOutcome` from `refresh_health_locked` and passes it to
  `review::handle`; when `DemandOutcome::validity() != "current"` the persistent
  side is unverified, so review emits `health_delta_incomparable { reason: <token> }`
  (the validity token) instead of a measured delta. `validity()` lives on
  `DemandOutcome` and is the shared seam with `file_health::attach_health_evidence`.
- Degraded files include `driving_findings` showing which on-demand
  biomarkers contributed to the decline
- **Known bug (sutra/411):** current coverage mirrors the snapshot's per-file
  decision (sutra/409), but incremental parsing deletes current findings and
  coverage without recording a snapshot. A formerly complete, unhealthy file
  can therefore be scored clean after a comment-only edit. The old rationale
  that every parse snapshots and preserves the same findings is false. Current
  evidence must establish current completeness; missing analysis must not be
  presented as measured improvement or degradation. See the review linked above.
- Review output ordering: constraint_violations → deviations →
  health_findings → hrr_shape_changes → health_delta (or
  health_delta_incomparable / health_delta_error)

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
| co_change_scatter / change_entropy / ownership_risk / hidden_coupling | git_metrics.rs (parse) | git-gated: NotARepo → Unsupported (excluded); NoHistory (empty window / `git log` failure) → Unwired (worst-cased) |
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
at the coupling cap). Instability is always computed so snapshot and file_health
component scores agree. Final clamp [1.0, 10.0].

Worst-casing (sutra/402 + sutra/408): `score_file(findings, facts, covered)`
deducts at full weight (within the category cap) for any file-scored biomarker
that is `Unwired` (no producer, or git NoHistory), OR `Scored` when the file's
analysis is not current (`!covered` — see the coverage section above), recording
each in `FileHealthScore.missing` and flipping `partial()`. This is live, not
dormant: any incrementally-reparsed or newly-added file is worst-cased until a
full parse recomputes its findings, and a git NoHistory workspace worst-cases
its git biomarkers. `Unsupported` dimensions (coverage_gradient always; git
biomarkers only when NotARepo) remain excluded and surfaced rather than scored
as zero debt.

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
- Integration tests: `tests/health-test.rs` (model, threshold,
  DB round-trip, waiver CRUD, waiver exclusion, scoring, git-organizational
  biomarkers: scatter, entropy, ownership, coupling, alias merging,
  snapshot per-file/per-component storage, file health history,
  trend comparison with file deltas, trend history mode, blame parsing,
  HealthFinding::to_row, health delta: degradation, improvement,
  no-snapshot fallback, on-demand finding attribution,
  component instability: basic, isolated, fully-efferent,
  file health: component filter, component instability in scores,
  import cycle: cyclic fires, acyclic absent, DB roundtrip)
- Integration tests: `tests/similarity_test.rs` (12 tests — HRR vectors,
  strip/embed modes, determinism, discrimination, pattern families,
  similarity search: strip mode, embed vs strip, self-exclusion, diagnostics)
- Test DB setup: `Db::open_unchecked("test", dir.path())` with tempdir
- Seed helpers: `seed_fn(db, file_id, qn, sn, max_nesting)`,
  `seed_commits(db, commits, pairs)` in health-test.rs
