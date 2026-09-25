# Health system architecture map

> **Archived (sutra/475).** The health layer is fully deleted (sutra/464,
> 473–475). History, complexity and similarity moved to
> [similarity-map.md](../similarity-map.md); what survived and why is in
> [health-disposition.md](../health-disposition.md).

Quick-reference for what remains of the health layer (erosion), plus the git
history ingestion and similarity modules it grew up alongside.

## Module layout

```
src/health/
  mod.rs            — `pub mod erosion` only
  erosion.rs        — Standalone erosion metric (NOT a biomarker, sutra/442):
                      mass/eroded/aggregate/nearest-rank percentiles,
                      select_samples (outermost + test exclusion) behind
                      samples_by_file (index) and parsed_samples (a fresh parse,
                      for review), COGNITIVE_THRESHOLD shared with diff_impact.

src/history.rs      — ingest(db, root, now): commit-file history against the
                      pinned HEAD and a UTC-day-quantized absolute cutoff
                      (window from components.toml, default 90 days). Confirmed
                      non-repo / unborn HEAD / empty window clear the tables;
                      probe failure, shallow clone or git log failure retain
                      prior rows. Returns {loaded, churn}; churn feeds
                      semantic anchors, commit_files feeds co-change (review
                      behavioral_coupling, component clustering).

src/parser/
  complexity.rs     — cyclomatic and cognitive (tree-sitter Node + src +
                      lang); classify_cognitive decides flow breaks and
                      nesting increments. Erosion reads cognitive.
  mod.rs            — ExtractedSymbol: cyclomatic, cognitive (Option<u32>),
                      computed for Function/Method kinds in every parser

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
                      output.
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

src/pipeline.rs     — post_parse_sequence: history::ingest (churn feeds semantic
                      anchors). A NoChanges parse re-ingests history
                      (refresh_history), then re-clusters if membership went
                      stale (sutra/443), then copies parse aggregates forward
                      into its checkpoint.
```

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

## Database

Dropped in 0081 (sutra/473): health_runs, health_current,
health_snapshot_files, health_snapshot_components,
health_snapshot_component_members, snapshots.health_score and
snapshots.health_run_id. Dropped in 0082: index_meta.index_epoch and
index_meta.git_availability. Dropped in 0083 (sutra/474): health_findings,
health_coverage; in 0084: health_waivers (Durable — user-authored waivers
were dropped with the feature); in 0085: symbols.max_nesting (only the
nested_complexity biomarker read it; the parsers no longer compute it). `snapshots` stays as the parse record
(`last_parse_time` / `last_parse_info`, erosion aggregates).

## Test locations

- Unit tests: `src/history.rs` (cutoff, window config), `src/parser/complexity.rs`
  (cyclomatic, cognitive), `src/graph.rs` (SCC)
- Real-path: `tests/history-ingest-test.rs` (loaded / shallow / git log failure /
  non-repo / unindexed-only history, unchanged-parse ingestion and re-clustering)
- Integration: `tests/erosion-test.rs` (selection, snapshot persistence, review
  erosion delta), `tests/similarity_test.rs`
