# Health system architecture map

> **Being deleted (sutra/464).** Scoring, trend, health runs, snapshot health
> detail, `sutra_file_health`, `sutra_trend`, review `health_delta` and the
> on-demand biomarkers were removed in sutra/473. The biomarkers, findings and
> waivers go in sutra/474 and erosion in sutra/475, after which this map is
> archived. See [health-disposition.md](health-disposition.md) for what
> survives and why. Don't extend these surfaces.

Quick-reference for what remains of the health layer. Git history ingestion,
which the git biomarkers consume, lives in `src/history.rs`.

## Module layout

```
src/health/
  mod.rs            — re-exports from findings and git_metrics
  findings.rs       — HealthFinding, BiomarkerKind (8 variants + ALL + parse +
                      needs_history), HealthSeverity (Advisory, Informational —
                      never Blocking), compute_nested_complexity,
                      compute_dead_code_ratio, compute_all_health_findings,
                      refresh_findings (recompute + replace the live table;
                      drops git-producer findings when history did not load)
  erosion.rs        — Standalone erosion metric (NOT a biomarker, sutra/442):
                      mass/eroded/aggregate/nearest-rank percentiles,
                      select_samples (outermost + test exclusion) behind
                      samples_by_file (index) and parsed_samples (a fresh parse,
                      for review), COGNITIVE_THRESHOLD shared with diff_impact.
  git_metrics.rs    — git-organizational biomarkers consuming commits +
                      commit_files. compute_co_change_scatter,
                      compute_change_entropy, compute_ownership_risk,
                      compute_hidden_coupling, compute_blast_radius_churn.
                      OwnersConfig + load_owners for .sutra/owners.toml
                      (malformed/unreadable → None → ownership not scored).

src/history.rs      — ingest(db, root, now): commit-file history against the
                      pinned HEAD and a UTC-day-quantized absolute cutoff
                      (window from components.toml, default 90 days). Confirmed
                      non-repo / unborn HEAD / empty window clear the tables;
                      probe failure, shallow clone or git log failure retain
                      prior rows. Returns {loaded, churn}.

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
                      (hidden_coupling and review behavioral_coupling).
  health.rs         — HealthFindingRow, HealthWaiverRow, NestingExceedRow.
                      Db methods: symbols_exceeding_nesting, replace_health_findings,
                      get_health_findings (optional file_id + biomarker_kind
                      filters), get_health_waivers, create_health_waiver
                      (upsert), delete_health_waiver,
                      get_health_findings_with_waiver_status, symbol_labels.

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
  similar.rs        — MCP tool: sutra_similar(symbol, mode, limit, threshold).
                      Resolves symbol → HRR vector, linear scan cosine
                      similarity, returns ranked matches with file locations.

src/db/
  similarity.rs     — HrrSymbolRow, SymbolSummary, PatternFamily types.
                      Db methods: function_symbols_for_hrr(_files),
                      insert_hrr_vectors_and_hashes, load_hrr_vector (single),
                      load_all_vectors_by_mode, replace_pattern_families,
                      query_pattern_families, symbols_by_ids,
                      function_symbol_count, delete_embed_vectors.
                      (hrr_codebook table dropped by migration 0064.)
  components.rs     — component_members_with_line_count() added for
                      NLOC-weighted component health scoring.

src/pipeline.rs     — post_parse_sequence: history::ingest (churn feeds semantic
                      anchors), then after conventions
                      health::refresh_findings. A NoChanges parse re-ingests
                      history and refreshes findings (refresh_history), then
                      re-clusters if membership went stale (sutra/443), then
                      copies parse aggregates forward into its checkpoint.
```

## Key types

### HealthFinding (health/findings.rs)
Core finding struct all biomarkers produce. Fields: `file_id: i64`,
`symbol_id: Option<i64>`, `biomarker_kind: BiomarkerKind`,
`severity: HealthSeverity`, `confidence: f64`, `provenance: String`,
`metric_value: f64`, `threshold: f64`, `detail: String`.

### BiomarkerKind (health/findings.rs)
Persistent producers only: `NestedComplexity`, `CoChangeScatter`,
`ChangeEntropy`, `OwnershipRisk`, `HiddenCoupling`, `ImportCycle`,
`DeadCodeRatio`, `BlastRadiusChurn`. `needs_history()` marks the five git
producers. `as_str()`/`parse()` roundtrip the snake_case DB form.

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
| health_coverage | Ephemeral | 0072 | Vestigial since sutra/416 (no reader or writer); goes in 474 |
| symbols (max_nesting col) | Ephemeral | 0027 | ALTER TABLE adds max_nesting INTEGER |

Dropped in 0081 (sutra/473): health_runs, health_current,
health_snapshot_files, health_snapshot_components,
health_snapshot_component_members, snapshots.health_score and
snapshots.health_run_id. Dropped in 0082: index_meta.index_epoch and
index_meta.git_availability. `snapshots` stays as the parse record
(`last_parse_time` / `last_parse_info`, erosion aggregates).

## Waiver mechanism

Parallel to constraint waivers, not shared tables:
- Identity key: `(biomarker_kind, file_path, COALESCE(symbol_qualified_name, ''))`
- Upsert on conflict (updates rationale, waived_by, updated_at)
- Matching in `get_health_findings_with_waiver_status`: joins findings to
  waivers via file path lookup, returns `Vec<(HealthFindingRow, bool)>`
- Waived findings are visible but flagged

No MCP tool for health waivers yet. Internal API only.

## Pipeline integration

```
parse_workspace
  └── post_parse_sequence
        └── ... ref resolution, graph rollups ...
        └── history::ingest → commits + commit_files, churn map
        └── ... components, anchors, aliases, HRR, constraints, conventions ...
        └── health::refresh_findings(db, root, history_loaded)
              └── compute_all_health_findings (nested, git biomarkers, import
                  cycle, dead code ratio, blast radius churn)
              └── replace_health_findings — DELETE + INSERT all
  └── NoChanges: refresh_history (ingest + refresh_findings), recluster_unchanged
  └── record_snapshot / record_unchanged_snapshot → insert_snapshot (prunes to 30)
```

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
input.

- `mass = cognitive × sqrt(end_line − start_line + 1)`; eroded when
  `cognitive >= COGNITIVE_THRESHOLD` (15). The constant is shared with
  `diff_impact`'s risk gate.
- Only outermost complexity-bearing symbols count (no ancestor with a
  non-null cognitive): the complexity walkers descend into nested functions,
  so a nested JS/TS function is already inside its parent's score.
- Test code is excluded: files matching `components::is_test_file`, and
  symbols (or ancestors) with `FLAG_TEST` (0x01, all parsers) or 0x02
  (`cfg(test)`/test path in Rust and Dart only — TypeScript uses 0x02 for
  `override`). Free `#[cfg(test)]` items are flagged too (sutra/445).
- Scopes (file, component, workspace) always SUM function masses. Workspace
  sums over files, not components (multi-membership). Empty scope → null
  share and percentiles. Rank/trend by absolute `eroded_mass`; `eroded_share`
  is descriptive only (non-monotone, bimodal at component scope).
- Persistence: `snapshots.eroded_mass/total_mass/erosion_version`, NULLABLE
  (migration 0079): pre-metric checkpoints read null, never 0. Computed in
  `compute_parse_aggregates`; a NoChanges parse copies it forward only when
  the previous checkpoint has the current `EROSION_VERSION`, otherwise
  recomputes. Bump `EROSION_VERSION` on any change to which symbols count,
  including parser-side changes (test flags, cognitive, parent links) — the
  parser stamp forces a reparse but does not recompute copied checkpoints.
- Review delta (`tools/erosion_delta.rs`, sutra/451): `sutra_review`'s
  `erosion_delta` block parses the base and head of every changed file where
  either side has a language adapter. Base is read from `old_path` for renames, via
  `review::resolve_diff_entries`. Each side is parsed by its own path's adapter
  (`side_adapters`), as the index would have parsed it: a rename across
  extensions changes the grammar, and a side with no adapter is `Unindexed`
  (no samples, like the index), so erosion renamed into an indexed language is
  added and erosion renamed out of one is deleted. Head is a revision, the index, or the
  worktree (`git::file_content_on_side`). Samples come from
  `erosion::parsed_samples` over `parser::persist::flatten_symbols_for_insert`,
  the flattening the index persists, so a function's sample equals the
  indexed one for the same bytes (pinned by
  `review_samples_match_indexed_samples`). Pairing: a
  `(qualified_name, kind)` key naming exactly one function on each side of a
  file pairs directly. A repeated key (cfg-gated twins) is ambiguous, so its
  group is first resolved within itself by `symbol_diff::resolve_renames`
  (`ResolveResult.pairs`: body identity, then same-name similarity). Only then
  do the remaining functions join the leftovers of all files for rename and
  move resolution. Order matters: the Rust structural hash ignores names, so
  resolving globally first let an unrelated new function with a twin's old
  structure claim that twin. Pairing twins by position first fabricated
  crossings when unchanged twins were reordered.
  **Known limit:** twins that identity cannot separate fall back to source
  order. If same-named twins are reordered *and* both are rewritten past the
  similarity gates (`MIN_COUNT_RATIO`) in one diff, the fallback can mispair
  them and report a spurious `crossed_up`/`crossed_down` pair. File and total
  mass stay correct; only per-function labels are wrong. It isn't fixed because
  `SymbolSpan` starts at the fn node, so the cfg attribute that would separate
  the twins is outside it. The fixes are widening spans to include attributes
  (which touches every hash consumer) or reporting the group as unresolved
  (new output vocabulary). Revisit if it shows up in practice. That means a rename
  that also rewrites the body is reported as deleted + added, the same as
  symbol_diff. A cross-file move is removed from its base file and added to its
  head file. Per-file `eroded_mass_added − eroded_mass_removed` equals the net
  change. A side that cannot be read or parsed, or is over `MAX_LINES`, makes
  the file `unavailable` (excluded from pairing and from totals, block `status`
  = `partial`). Syntax errors mark the file `partial`. Complete files with no
  eroded mass on either side are omitted. `FunctionDelta.marginal_gain` is the sutra/404
  hook: `None` today.

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

## Test locations

- Unit tests: `src/history.rs` (cutoff, window config), `src/health/git_metrics.rs`
  (owners config), `src/parser/complexity.rs` (nesting depth), `src/graph.rs` (SCC)
- Real-path: `tests/history-ingest-test.rs` (loaded / shallow / git log failure /
  non-repo / unindexed-only history, unchanged-parse ingestion and re-clustering)
- Integration: `tests/health-test.rs` (finding model, nesting threshold, DB
  round-trip, waivers, git-organizational biomarkers, dead code ratio, blast
  radius churn, import cycle, snapshot pattern-family round trip)
- Integration: `tests/erosion-test.rs` (selection, snapshot persistence, review
  erosion delta), `tests/similarity_test.rs`
