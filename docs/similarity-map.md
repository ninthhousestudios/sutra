# Similarity, history and complexity map

Quick-reference for git history ingestion, per-symbol complexity and the HRR
similarity modules. Split out of the archived health map when the health layer
was deleted (sutra/464, sutra/473–475; see
[health-disposition.md](health-disposition.md) for what survived and why).

## Module layout

```
src/history.rs      — ingest(db, root, now): commit-file history against the
                      pinned HEAD and a UTC-day-quantized absolute cutoff
                      (window from components.toml, default 90 days). Confirmed
                      non-repo / unborn HEAD / empty window clear the tables;
                      probe failure, shallow clone or git log failure retain
                      prior rows. Returns {loaded, churn}; churn feeds
                      semantic anchors, commit_files feeds co-change (review
                      behavioral_coupling, component clustering). Each commits
                      row carries file_count = every path git reported, indexed
                      or not (migration 0087).

src/db/graph.rs     — cochange_pairs_above_threshold: jaccard over commits whose
                      file_count <= MAX_COCHANGE_COMMIT_FANOUT (30; NULL falls
                      back to the indexed count), so sync drops and sweeps carry
                      no co-edit signal. static_file_edges = resolved refs ∪
                      resolved imports (incl. `mod x;`).
src/tools/review.rs — behavioral_coupling: partners with no static edge, same
                      test-ness, >= MIN_PARTNER_SHARED_COMMITS (2) shared
                      commits. Errors surface as behavioral_coupling_error.
                      Measurement: behavioral-coupling-backtest.md (sutra/476).

src/parser/
  complexity.rs     — cyclomatic and cognitive (tree-sitter Node + src +
                      lang); classify_cognitive decides flow breaks and
                      nesting increments. diff_impact and sutra_hotspots read
                      cognitive; diff_impact owns COGNITIVE_THRESHOLD (15).
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

## Database

`snapshots` is the parse record behind `last_parse_time` / `last_parse_info`,
plus the parse aggregates (total_complexity, dead/hotspot/pattern-family
counts) a NoChanges parse copies forward. The health tables and columns that
used to hang off it were dropped in 0081–0086 (sutra/473–475).

## Test locations

- Unit tests: `src/history.rs` (cutoff, window config), `src/parser/complexity.rs`
  (cyclomatic, cognitive), `src/graph.rs` (SCC)
- Real-path: `tests/history-ingest-test.rs` (loaded / shallow / git log failure /
  non-repo / unindexed-only history, unchanged-parse ingestion and re-clustering)
- Integration: `tests/similarity_test.rs`
