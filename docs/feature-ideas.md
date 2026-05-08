# sutra feature ideas

Survey of adjacent code intelligence tools — fallow (TS/JS), jcodemunch
(polyglot tree-sitter MCP), token-reducer (BM25+vector context compression) —
adapted for sutra's Rust/Dart, MCP-first, daemon-backed model.

Organized by **what it would take to build**, not by raw value/effort. The
"straightforward" bucket is shovel-ready against existing data; the "design
work" bucket has real open questions that need answers before code.

---

## Already shipped

These were on the original fallow list and are now in tree:

- `sutra_dead` — unused symbols/files
- `sutra_hotspots` — churn × complexity
- `sutra_file_health` — composite maintainability score
- `sutra_diff_impact` — git diff → impacted symbols
- `sutra_cochange` — co-change history
- `sutra_calls` / `sutra_refs` — call graph traversal
- `sutra_impact` / `sutra_deps` — forward/back impact

---

## Straightforward — substrate exists, shape is clear

### sutra_trend (was fallow #5)

Extend the existing snapshot table to record aggregate metrics (total
complexity, dead-symbol count, hotspot count, workspace health) per parse run.
Add `sutra_trend(from, to)` that returns deltas.

**Why straightforward:** snapshots already exist with timestamps. This is a
schema extension and a diff function.

**Useful for:** CI gates ("fail if health dropped"), pre/post-refactor checks.

---

### sutra_pr_risk

Composite risk score (0.0–1.0) for a branch/PR — fuses blast radius,
complexity delta, churn, and change volume. Sutra has every input via
`sutra_diff_impact`, `sutra_file_health`, `sutra_hotspots`.

**Why straightforward:** weights + a formula, same shape as `sutra_file_health`.
The hard part (collecting signals) already exists.

**Tool shape:** `sutra_pr_risk(base, head)` → score, breakdown per signal,
top-N riskiest changed symbols, recommendations.

---

### sutra_provenance

Git archaeology for a single symbol: walk every commit that touched the
symbol's byte range, classify each (creation / bugfix / refactor / feature /
perf / rename / revert) from commit-message heuristics, emit a narrative.

**Why straightforward:** symbol → file:byte-range is already known; commit
classification is a small rule table (`fix:` prefix → bugfix, etc.); narrative
is templated. Worst case the heuristics are mediocre — graceful degradation.

**Tool shape:** `sutra_provenance(symbol)` → ordered commit list with
classification + author + summary string.

---

### Per-result freshness + confidence

Attach freshness state to every result entry:
`fresh | edited_uncommitted | stale_index`. Compare per-file mtime to last
parse timestamp. Aggregate into a `_meta.freshness` summary on each response.

Confidence is narrower — only meaningful for ranked queries
(`sutra_find`, `sutra_grep`). Compute from top-1/top-2 score gap and
identity-match presence.

**Why straightforward:** freshness is plumbing — no new analysis, just
threading mtime through response builders. Confidence is opt-in per tool.

**Effort:** maybe a day for freshness, another day for confidence on the
ranked tools.

---

### sutra_signal_chains / trace (covers fallow #3)

Forward and backward traversal of the call graph from entry points to
arbitrary symbols, and vice versa. Subsumes the fallow trace idea and extends
it with explicit entry-point modeling.

**Entry-point detection (rule-based):**
- Rust: `fn main`, `#[test]`, `#[tokio::main]`, `#[wasm_bindgen]`, public
  items in `lib.rs`, `#[bench]`
- Dart: `void main()`, route handlers, test bodies

**Tool shape:**
- `sutra_trace(symbol, direction=forward|backward)` → chains to/from the
  symbol
- For dead-code explanation: prove no path from any entry-point reaches X.

**Why straightforward:** call graph already exists; entry-point detection is
a small rule list per language; traversal is BFS with cycle detection. The
*output rendering* is the design knot (see below).

---

### sutra_winnow (multi-axis composite query)

One call that AND-intersects existing signals. Today an agent asking
"functions calling `db::exec`, complexity > 10, churned in last 30d, ranked
by PageRank" makes 4 calls and merges by hand.

**Tool shape:**
```
sutra_winnow({
  calls_to: "db::exec",
  complexity: { gt: 10 },
  churn: { since: "30d", gt: 5 },
  rank_by: "pagerank"
})
```

**Why straightforward:** every axis already exists as its own tool. Just
intersect the result sets. Schema design is small. Operators per axis (`gt`,
`in`, `matches`) are obvious.

**Why it matters:** materially fewer round-trips per agent task.

---

## Needs design work

### sutra_plan_refactoring

Generate `{old_text, new_text}` edit blocks for rename / move / extract /
signature change, with import rewrites and collision detection.

**Open questions:**
- Scope: rename only first, or all four operations?
- Trait/generic coordination — renaming a trait method requires updating
  every `impl` block and all call sites. Tree-sitter sees these but emitting
  *correct byte ranges* across hundreds of sites is fragile.
- Macro-generated code is invisible to tree-sitter. How do we warn the
  agent when a rename probably extends into macro output?
- Cross-crate renames in a cargo workspace — feasible. Cross-crate to
  *external* downstream crates — out of scope.
- What's the rollback contract if half the edits fail to apply?

**Why design-heavy:** the output format (`{old,new}`) is trivial. Correctly
*identifying every site that needs changing* across Rust's macro, generic,
and trait systems is the work.

---

### sutra_search_ast (cross-language structural patterns)

Preset detectors for anti-patterns (empty catches, deeply nested control
flow, god functions, magic numbers, TODO/FIXME, unwrap sprawl in non-test
code, etc.) plus optional custom queries.

**Open questions:**
- Rule format: hardcoded Rust functions per pattern (clean, proliferates) vs.
  tree-sitter query files (declarative, verbose, language-coupled) vs. mini-
  DSL (jcodemunch's path — flexible, you're inventing a language).
- Which presets are actually worth shipping for Rust+Dart? Generic
  cross-language presets ("god function") translate; language-specific ones
  (`unwrap()` outside tests, `?` in `main`, missing `#[must_use]`) are where
  the value is.
- Cross-language node-type abstraction — jcodemunch claims "universal node
  type mapping." That's a big project. Skip it; ship per-language rule sets.

**Recommendation:** ship ~10 curated Rust + ~5 Dart presets as hardcoded
functions first. Mini-DSL only if usage justifies it.

---

### Compact wire format (MUNCH-style)

Path interning + packed homogeneous rows, claimed ~45% byte savings on
graph/outline responses.

**Open questions:**
- Wire compatibility — current MCP clients expect JSON. Negotiate via a
  `format=compact|auto|json` arg per tool? Auto-threshold (only when savings
  ≥ 15%)?
- Encoder/decoder symmetry — agents need to decode it, which means either
  the harness understands it or we ship a decoder skill.
- Is response size *actually* a pain point today? Sutra's responses are
  generally small. Premature until we see a tool whose output is
  measurably bloating context.

**Recommendation:** defer. Revisit if `sutra_map` or `sutra_calls` start
hitting agent context budgets.

---

### sutra_boundaries (fallow #8)

Architectural-zone enforcement. Define zones via glob/module patterns,
declare allowed dependency directions, flag violating imports.

**Open questions:**
- Config schema — `.sutra.toml` at workspace root? What does a zone
  definition look like (paths? crate names? module prefixes?)?
- Rule vocabulary — only `may_not_import`, or also `must_export_through`,
  `private_to_zone`, `test_only`?
- Preset packs for Rust patterns: crate layering, test isolation, mod-root-
  only-exports.
- How to surface violations — separate tool, or fold into `sutra_dead` and
  `sutra_pr_risk`?

**Why design-heavy:** the analysis (graph traversal flagging cross-zone
edges) is trivial. The schema *is* the feature.

---

### sutra_dupes (fallow #6)

Clone detection.

**Open questions:**
- Algorithm: hash-only exact-match first cut vs. suffix array for
  partial/fuzzy matches.
- Normalization strictness — whitespace? identifiers? literals?
- Minimum clone size — token count or line count?
- Clone families (transitive grouping) vs. pairwise reports.
- Cross-file only or include same-file repetition?

**Why design-heavy:** every choice above changes both implementation effort
and result quality dramatically. Largest scope on this list.

**Recommendation:** start with function-body-token-hash exact matches. That
catches copy-pasted functions — the most common case — at fraction of the
effort. Suffix array later if needed.

---

### sutra_refactor_targets (fallow #7)

Ranked refactoring recommendations combining complexity, coupling, churn,
dead code, dupes, and boundary violations.

**Open questions:**
- Effort taxonomy — "low/medium/high" needs concrete rules.
- Recommendation types — splitting, extracting, deleting, deduplicating —
  each is its own classifier.
- Ranking formula across heterogeneous signals.

**Why design-heavy:** depends on `sutra_dupes` and `sutra_boundaries`
existing first. Then it's a presentation layer over their output, but the
ranking formula is opinionated and needs iteration.

---

### Trace output format

Sub-design problem of `sutra_signal_chains`. How to render a chain readably
in JSON when:
- a symbol is reachable from 100+ entry points (truncate? cluster by entry-
  point category?)
- the graph has cycles (mark them and stop, or show the cycle?)
- chains fan out wide mid-traversal (paths-only vs. tree-of-callers)

**Recommendation:** ship a single canonical shape (top-N shortest paths to/
from entry points, configurable N, cycles marked) and iterate on feedback.

---

## Lower priority / questionable

- **Tectonic map** (jcodemunch) — fuse imports + shared refs + git
  co-churn into module topology. Visually interesting; hard to render
  usefully through MCP. Better as a CLI/visual feature later.
- **audit_agent_config** (jcodemunch) — scan CLAUDE.md for stale symbol
  references. Clever, narrow scope. Ship only if you find yourself burned by
  stale references in practice.
- **Tool tiering / compact schemas / disabled_tools** (jcodemunch) —
  schema-token control. Sutra has ~13 tools today. Re-evaluate at 25+.
- **2-hop symbol expansion on read** (token-reducer) — auto-pull referenced
  symbols. Helpful for retrieval-style agents; less obvious win for sutra's
  structural-query model where the agent asks for what it wants.
- **Hybrid BM25 + vector retrieval** (token-reducer) — semantic search
  shape, not structural-query shape. Smriti / qartez territory.
- **Auto secret redaction in responses** (jcodemunch) — verify smriti's
  existing secret gating already covers the sutra path before duplicating.

---

## Suggested implementation order

Across both buckets, ordered by value/effort within each:

**Round 1 — straightforward wins:**
1. Per-result freshness + confidence (1-2 days, every existing tool benefits)
2. `sutra_winnow` (cuts agent round-trips immediately)
3. `sutra_pr_risk` (small layer over `sutra_diff_impact`)
4. `sutra_trend` (extends snapshot infra)
5. `sutra_signal_chains` / trace (covers fallow #3, also explains dead code)
6. `sutra_provenance`

**Round 2 — pick based on actual pain:**
7. `sutra_search_ast` with curated presets (start hardcoded, no DSL)
8. `sutra_plan_refactoring` — rename only first, expand later
9. `sutra_boundaries` — schema design first, then implementation

**Round 3 — bigger bets:**
10. `sutra_dupes` — start with hash-only exact-match
11. `sutra_refactor_targets` — depends on dupes + boundaries

Skip / defer: compact wire format, tectonic map, tool tiering — re-evaluate
when there's a concrete pain point.
