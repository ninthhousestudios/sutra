# Health system architecture map

Quick-reference for agents planning or implementing health/similarity tasks.
Read this first, then do targeted `sutra_outline` / `sutra_read` calls on
specific files. Updated after each health-system landing.

Last updated: 2026-06-05 (5e-5: structural similarity search)

## Module layout

```
src/health/
  mod.rs            — re-exports from findings, git_metrics, and scoring
  findings.rs       — HealthFinding, BiomarkerKind (13 variants + from_str),
                      HealthSeverity (Advisory, Informational — never Blocking,
                      + from_str), compute_nested_complexity,
                      compute_all_health_findings(db, workspace_root)
  git_metrics.rs    — git-organizational biomarkers consuming commits +
                      commit_files tables. compute_co_change_scatter,
                      compute_change_entropy, compute_ownership_risk,
                      compute_hidden_coupling. OwnersConfig + load_owners_config
                      for .sutra/owners.toml alias mapping.
  scoring.rs        — HealthCategory (5 variants), category caps, biomarker
                      weights (repowise calibrated), severity weights,
                      score_file (category capping + proportional scaling),
                      score_component (NLOC-weighted average),
                      FileHealthScore, FindingDeduction

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
                      Db methods: symbols_exceeding_nesting, replace_health_findings,
                      get_health_findings (optional file_id + biomarker_kind filters),
                      get_health_waivers, create_health_waiver (upsert),
                      delete_health_waiver, get_health_findings_with_waiver_status.
  mod.rs            — TABLE_REGISTRY entries: health_findings (Ephemeral),
                      health_waivers (Durable). SymbolRow.max_nesting field.
                      InsertSymbolParams.max_nesting field.
  migrations.rs     — 0027 (ephemeral), 0028 (durable)

src/similarity/
  hrr.rs            — HrrVec (1024-dim), Complex, FFT-based circular
                      convolution, Rng (deterministic xoshiro256++).
                      Key methods: cosine_similarity, bind/unbind,
                      bundle, permute, to_bytes/from_bytes.
  codebook.rs       — Codebook: maps AST node-kind strings to random
                      HrrVec. Persists to hrr_codebook table (durable).
  encoder.rs        — encode_subtree(node, source, codebook, embed_idents).
                      embed_idents=false → strip mode (structure only),
                      embed_idents=true → embed mode (structure + names).
  duplicates.rs     — find_pattern_families: union-find clustering over
                      strip vectors. Used by sutra_duplicates tool.
  search.rs         — find_similar: cosine-similarity ranked search.
                      SimilarityMatch{symbol_id, score}. Self-exclusion,
                      threshold filtering, limit truncation.
  mod.rs            — compute_hrr_vectors (pipeline entry),
                      compute_pattern_families.

src/tools/
  file_health.rs    — MCP tool: queries findings with waiver status, scores
                      via scoring::score_file, builds per-file + per-component
                      JSON. Legacy compute_file_scores still present for
                      pipeline.rs (removed when sutra/91 lands).
  similar.rs        — MCP tool: sutra_similar(symbol, mode, limit, threshold).
                      Resolves symbol → HRR vector, linear scan cosine
                      similarity, returns ranked matches with file locations.

src/db/
  similarity.rs     — HrrSymbolRow, SymbolSummary, PatternFamily types.
                      Db methods: function_symbols_for_hrr, replace_hrr_vectors,
                      load_hrr_codebook, save_hrr_codebook_entries,
                      load_all_strip_vectors, load_hrr_vector (single),
                      load_all_vectors_by_mode, replace_pattern_families,
                      query_pattern_families, symbols_by_ids.
  components.rs     — component_members_with_line_count() added for
                      NLOC-weighted component health scoring.

src/pipeline.rs     — post_parse_sequence tail: compute_all_health_findings
                      + replace_health_findings (runs after component discovery
                      and alias sync, before record_snapshot)
```

## Key types

### HealthFinding (health/findings.rs)
Core finding struct all biomarkers produce. Fields: `file_id: i64`,
`symbol_id: Option<i64>`, `biomarker_kind: BiomarkerKind`,
`severity: HealthSeverity`, `confidence: f64`, `provenance: String`,
`metric_value: f64`, `threshold: f64`, `detail: String`.

### BiomarkerKind (health/findings.rs)
Enum with 13 variants. Currently implemented: `NestedComplexity`.
Stubs for future: `CoChangeScatter`, `ChangeEntropy`, `OwnershipRisk`,
`FunctionHotspot`, `HiddenCoupling`, `BlastRadiusChurn`, `DeadCodeRatio`,
`CodeAgeVolatility`, `CoverageGradient`, `ConventionDrift`,
`ComponentInstability`, `HrrShapeChange`.

`as_str()` returns snake_case DB representation. `from_str()` roundtrips.
`default_severity()` maps tier 1/2 → Advisory, tier 3 + sutra-specific →
Informational. `category()` returns HealthCategory. `default_weight()`
returns repowise-calibrated weight (or moderate/uncalibrated default).

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
        └── replace_health_findings(findings) — DELETE + INSERT all
  └── record_snapshot (unchanged — health score still uses old model)
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
| Sutra | Informational | convention_drift, component_instability, hrr_shape_change | uncalibrated |

### Health scoring (sutra/85, implemented)

`health/scoring.rs`: base 10.0, deductions per finding
(`severity.weight() × biomarker.default_weight()`), capped per category:

| Category | Cap | Biomarkers |
|---|---|---|
| organizational | -3.5 | co_change_scatter, change_entropy, ownership_risk |
| structural | -2.5 | nested_complexity, function_hotspot, blast_radius_churn |
| coupling | -2.0 | hidden_coupling, component_instability |
| freshness | -1.5 | code_age_volatility, hrr_shape_change |
| coverage | -2.0 | dead_code_ratio, coverage_gradient, convention_drift |

Severity weights: Advisory = 1.0, Informational = 0.5.
Proportional scaling within category when sum exceeds cap.
Component scores: NLOC-weighted average of member file scores.
Final clamp [1.0, 10.0].

Calibrated biomarker weights (from repowise T0-protocol corpus):
co_change_scatter 1.80, change_entropy 1.51, ownership_risk 1.38,
nested_complexity 1.34, function_hotspot 1.16, code_age_volatility 1.10.
Non-repowise defaults: hidden_coupling 1.00, blast_radius_churn 1.00,
dead_code_ratio 0.80, coverage_gradient 0.80. Uncalibrated: convention_drift
0.50, component_instability 0.50, hrr_shape_change 0.50.

The `file_health` MCP tool now returns findings + derived scores (1.0–10.0
scale). Legacy `compute_file_scores` (0–100 scale) remains for
`pipeline.rs::compute_snapshot_aggregates` until sutra/91 rewires it.

### Remaining arc tasks

| Task | Title | Status | Key concern |
|---|---|---|---|
| sutra/84 | health finding model + first biomarker | done | this doc |
| sutra/85 | health scoring with category capping | done | scoring.rs + tool rewrite |
| sutra/86 | git-organizational biomarkers | done | git_metrics.rs, db/graph.rs queries |
| sutra/87 | review-1: health foundation | ready-for-human | review gate |
| sutra/88 | HRR encoder | done | similarity/hrr.rs, encoder.rs, codebook.rs |
| sutra/89 | structural similarity search | done | similarity/search.rs, tools/similar.rs |
| sutra/90 | pattern families + duplicates | done | similarity/duplicates.rs, tools/duplicates.rs |
| sutra/93 | semantic diff for review | ready-for-agent | HRR vectors |

## Test locations

- Unit tests: `#[cfg(test)]` in `src/parser/complexity.rs` (5 nesting depth tests)
- Unit tests: `#[cfg(test)]` in `src/similarity/search.rs` (5 search tests)
- Integration tests: `tests/health-test.rs` (29 tests — model, threshold,
  DB round-trip, waiver CRUD, waiver exclusion, scoring, git-organizational
  biomarkers: scatter, entropy, ownership, coupling, alias merging)
- Integration tests: `tests/similarity_test.rs` (12 tests — HRR vectors,
  strip/embed modes, determinism, discrimination, pattern families,
  similarity search: strip mode, embed vs strip, self-exclusion, diagnostics)
- Test DB setup: `Db::open_unchecked("test", dir.path())` with tempdir
- Seed helpers: `seed_fn(db, file_id, qn, sn, max_nesting)`,
  `seed_commits(db, commits, pairs)` in health-test.rs
