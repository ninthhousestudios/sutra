# sutra v1 PRD

## Problem Statement

Sutra currently exposes ~20 individual code intelligence tools (impact, deps,
calls, refs, hotspots, etc.) that agents must compose manually. An agent
reviewing a PR has to call 4–8 tools, merge results by hand, and form its own
judgment about risk. There is no structured review workflow, no live maintained
views of codebase invariants, no convention detection, and query failures return
ambiguous empty results instead of actionable diagnostics.

The v1 spikes (sutra/v1/1–4) validated three new reasoning substrates — HRR for
associative ranking, differential dataflow for maintained graph views, and FCA
for convention detection — and eliminated one (Salsa). The substrates are proven
individually but not integrated into the product.

## Solution

Sutra v1 adds four capabilities that transform it from a bag of structural
tools into a review intelligence engine:

1. **`sutra_review`** — a composite review workflow that analyzes a diff,
   computes blast radius, checks conventions, scores risk, and returns a
   bounded, ranked review context in a single call.

2. **Differential dataflow maintained views** — live, automatically incremental
   graph views (dependency cycles, blast-radius rollups, forbidden dependency
   violations) that update as files change, powered by DD rebuilt from SQLite
   on first query.

3. **FCA-backed convention detection** — automatic extraction of codebase
   conventions from structural patterns, persisted with stable identities,
   checked against changed code during review, with developer-authored
   suppressions.

4. **Structured query diagnostics** — every symbol-oriented tool distinguishes
   "no such symbol" from "ambiguous" from "stale index" from "analysis
   disabled," with candidate suggestions and freshness metadata.

Plus `sutra_help` with agent-oriented recipes.

## User Stories

1. As an agent reviewing a PR, I want to call a single tool and get a complete
   review context (changed symbols, affected symbols, risk score, convention
   violations, recommended reads), so that I don't have to compose 4–8 tool
   calls manually.

2. As an agent reviewing a PR, I want the review output bounded to a fixed
   size with truncation metadata, so that large blast radii don't blow my
   context window.

3. As an agent reviewing a PR, I want a computed risk score (0.0–1.0) with a
   breakdown by signal (blast radius, complexity delta, convention violations,
   hotspot overlap, churn), so that I can form a calibrated judgment.

4. As an agent reviewing a PR, I want a ranked `recommended_reads` list
   (ordered by: convention violation site > high-risk affected symbol > changed
   symbol with high fan-in), so that I know what to read first.

5. As an agent, I want `sutra_review` to accept a diff source
   (`branch` | `staged` | `unstaged`, default `branch` against main), so that
   I can review at different stages of the workflow.

6. As an agent, I want dependency cycle detection to run continuously as files
   change, so that `sutra_review` can report "your change introduces a new
   cycle" without a full graph recomputation.

7. As an agent, I want blast-radius rollups maintained live, so that impact
   queries reflect the current graph state without re-running transitive
   closure on every call.

8. As a developer, I want to declare forbidden dependencies in
   `.sutra/rules.toml` (e.g., `src/tools/*` must not import `src/daemon.rs`),
   so that architectural boundaries are enforced continuously.

9. As a developer, I want forbidden dependency violations to surface in
   `sutra_review` as constraint violations, so that boundary breaks are caught
   during review.

10. As an agent, I want FCA to automatically discover codebase conventions
    (e.g., "all error types implement Display," "test files contain test
    functions"), so that I can check my changes against patterns I didn't know
    existed.

11. As a developer, I want to suppress false-positive conventions in
    `.sutra/rules.toml` (by stable hash or with per-symbol exemptions), so
    that noisy implications don't erode trust in the feature.

12. As an agent, I want convention violations to appear in `sutra_review`
    output as a separate section with different confidence level than
    structural constraint violations, so that I can weigh them appropriately.

13. As an agent, I want conventions extracted incrementally on each parse and
    persisted in SQLite, so that I can see "this convention is new" or "this
    convention disappeared" over time.

14. As an agent, when I query a symbol that doesn't exist, I want a structured
    `no_such_symbol` response with candidate qualified names and suggested next
    query, so that I don't conclude "no callers" when I just got the name
    wrong.

15. As an agent, when a symbol name is ambiguous (matches multiple qualified
    names), I want an `ambiguous` response listing all candidates with their
    kinds and files, so that I can refine my query.

16. As an agent, when the index is stale, I want a `stale_index` diagnostic
    with the staleness age and affected files, so that I know to trigger a
    reparse before trusting results.

17. ~~As an agent, when an analysis tier is disabled, I want an
    `analysis_tier_disabled` diagnostic explaining which tier and how to
    enable it, so that I don't mistake missing results for absent data.~~
    **Dropped** — the analysis tier was removed entirely; it gated no
    expensive work and hid no schemas, so all tools are now callable
    unconditionally and no such diagnostic is needed.

18. As an agent, I want `sutra_help` to return recipes for common workflows
    ("review my current diff," "find callers and affected tests," "check
    convention violations"), so that I can discover sutra's capabilities
    without reading docs.

19. As a developer, I want DD graph state to be lazy-populated on first
    DD-backed query and evicted after idle timeout, so that workspaces I'm not
    actively querying don't consume memory.

20. As an agent, I want the DD cold-start to be transparent (sub-second from
    SQLite rebuild), so that I don't notice whether the graph was warm or cold.

21. As a developer, I want sutra's schema migrations to use ordered IDs and
    content hashes, so that temporal and convention schema additions in v1 are
    safe and reversible.

22. As an agent, I want every tool response to include a `freshness` envelope
    (fresh | edited_uncommitted | stale_index) per result entry, so that I
    can trust or discount individual results.

## Implementation Decisions

### Architecture

- **SQLite remains the persistence layer and source of truth.** DD and FCA are
  in-memory derived state rebuilt from SQLite. No new persistence formats.

- **DD lives inside the daemon process.** Lazy-populated on first DD-backed
  query. Rebuilt from SQLite facts. Evicted after configurable idle timeout
  (default 30 minutes). `sutra_status` does not trigger DD population or
  expose DD state.

- **FCA extraction runs incrementally on each parse.** Results persisted in
  SQLite with stable identity hashes (hash of antecedent + consequent).
  Conventions survive daemon restarts.

- **Salsa is not used.** The spike (sutra/v1/4) showed it's redundant with
  SQLite content hashing for file-level memoization and doesn't help with
  cross-file aggregation.

### Module structure

Four new modules:

- **`src/dd/`** — DD engine. Owns the timely/DD worker thread, input
  collections, and probes. Interface: `ingest(facts)`, `update(delta)`,
  `query_cycles()`, `query_blast_radius(symbol)`,
  `query_forbidden_deps(rules)`. No DD types leak outside this module.

- **`src/fca/`** — FCA engine. Owns the formal context matrix, NextClosure
  implementation, implication mining, and suppression loading. Interface:
  `rebuild(symbols, attributes)`, `update_incremental(delta)`,
  `conventions()`, `check(symbols) -> Vec<Violation>`. Conventions are plain
  data structs.

- **`src/review.rs`** — review compositor. Orchestrates a review by calling
  git diff, feeding changed symbols through impact/DD/FCA, computing risk
  score, truncating lists, ranking recommended reads. Only module that knows
  about all pieces. DD and FCA don't know about each other.

- **`src/diagnostics.rs`** — structured error/ambiguity responses. `Diagnostic`
  enum with variants (`NoSuchSymbol`, `Ambiguous`, `Stale`,
  `AnalysisTierDisabled`, `PartialResolution`), plus `suggest_next_query()`.
  Retrofitted into existing tool handlers.

### DD maintained views (v1 scope)

Three views:

1. **Cycle detection** — flags dependency cycles. Review reports "your change
   introduces a new cycle" or "existing cycle includes these files."

2. **Blast-radius rollups** — transitive reachability count per symbol. Powers
   the risk score without re-running transitive closure per query.

3. **Forbidden dependency enforcement** — checks graph edges against authored
   rules from `.sutra/rules.toml`. Violations are structural constraint
   violations (zero false positives).

DD is **not used for**: PageRank (doesn't fit DD's monotone iterate model),
one-shot parameterized queries (imperative traversal is simpler), or anything
where extracting a DD result costs more than the traversal.

### FCA conventions (v1 scope)

- Symbol-level formal context only (the spike showed file-level needs import
  resolution, which isn't ready).
- Approximate implications at confidence >= 0.9.
- Filter known language-rule implications where feasible.
- Persist convention set in SQLite: `conventions` table with `id` (hash),
  `antecedent`, `consequent`, `support`, `confidence`, `first_seen`,
  `last_seen`, `suppressed` (boolean).
- Check changed symbols against active conventions during review.
- Convention violations are distinct from DD constraint violations in review
  output — different confidence semantics.

### Review output shape

```
ReviewResult {
  diff_source: "branch" | "staged" | "unstaged",
  base_ref: String,

  changed_files: Vec<ChangedFile>,          // all
  changed_symbols: Vec<ChangedSymbol>,      // all

  affected_files: Vec<AffectedFile>,        // top 20, truncated flag
  affected_symbols: Vec<AffectedSymbol>,    // top 20, truncated flag
  affected_total: usize,

  constraint_violations: Vec<Violation>,    // all (DD: cycles, forbidden deps)
  convention_violations: Vec<Violation>,    // all (FCA: pattern violations)

  risk_score: f64,                          // 0.0–1.0
  risk_breakdown: RiskBreakdown {
    blast_radius: f64,
    complexity_delta: f64,
    convention_violations: f64,
    hotspot_overlap: f64,
    churn: f64,
  },

  recommended_reads: Vec<ReadRecommendation>,  // top 10
  freshness: FreshnessEnvelope,
}
```

### Configuration

Single file: `.sutra/rules.toml`.

```toml
[constraints]
forbidden_deps = [
  { from = "src/tools/*", to = "src/daemon.rs" },
]

[conventions]
suppress = ["a1b4c2d1"]

[[conventions.exempt]]
convention = "e5f6g7h8"
symbols = ["InternalError"]
```

### Diagnostics

Every symbol-oriented tool response distinguishes:

- `no_such_symbol` — with candidate qualified names, indexed kinds,
  matching files
- `ambiguous` — with all matching qualified names
- `stale_index` — with staleness age and affected files
- `analysis_tier_disabled` — with tier name and enable instructions
- `partial_resolution` — resolved some but not all references
- `symbol_exists_with_no_results` — symbol found but query returned empty

Each diagnostic includes a `suggested_next_query` field.

### Migration

- Add `schema_migrations` table with ordered IDs and content hashes before
  adding convention/temporal tables.
- New tables: `conventions`, `convention_checks` (per-review run).
- Schema changes to `files` and `symbols` tables for freshness per-entry
  metadata.

## Testing Decisions

Good tests verify external behavior through the module's public interface.
They don't test internal data structures or implementation details.

### Modules under test

**DD engine (`src/dd/`):**
- Given a set of facts from a known codebase fixture, verify cycle detection
  returns expected cycles.
- Given facts, verify blast-radius rollup counts match imperative computation.
- Given forbidden dep rules and a graph with violations, verify violations are
  reported. Given a clean graph, verify no violations.
- Given an initial fact set, apply a delta (add/remove edges), verify views
  update correctly.
- Verify eviction: after idle timeout, DD state is released and re-query
  triggers rebuild with same results.

**FCA engine (`src/fca/`):**
- Given a known symbol-attribute matrix, verify extracted conventions match
  expected implications.
- Given conventions and a set of changed symbols, verify violations are
  detected.
- Given suppressions, verify suppressed conventions don't produce violations.
- Given an incremental update (symbol added/removed), verify conventions
  update correctly.
- Verify stable identity hashes: same implication produces same hash across
  rebuilds.

**Review compositor (`src/review.rs`):**
- Given a workspace with known diff, verify review output contains expected
  changed files/symbols, affected files/symbols, and risk score is in
  expected range.
- Verify truncation: given a diff with > 20 affected symbols, verify
  `affected_symbols` is capped at 20 with `affected_total` showing true count.
- Verify `recommended_reads` ordering: convention violation sites rank above
  plain affected symbols.
- Verify constraint violations and convention violations appear in separate
  sections.
- Verify freshness envelope reflects actual index state.

### Prior art

Existing tests in `tests/` follow a pattern of workspace fixtures with
known source files, parsed into a temp DB, then queried through tool handlers.
`tests/impact_test.rs`, `tests/pipeline-test.rs`, and `tests/pr-risk-test.rs`
are the closest analogues for the review compositor tests.

## Out of Scope

- **Temporal snapshots / SHA-associated parse state** — v1.1 (milestone 5).
  v1 reviews working-tree state only.
- **HRR-based ranking of review results** — v1.1 (milestone 6). v1 uses
  structural ranking only.
- **MCP facade consolidation** (collapsing tools into `sutra_graph`,
  `sutra_query`, etc.) — v1.1 (milestone 7). Existing tools remain as-is.
- **File-level FCA contexts** — need import resolution improvements first.
- **Cloud embeddings or LLM-based analysis** — sutra remains local-first.
- **Salsa integration** — spike verdict was "skip."
- **PageRank in DD** — doesn't fit DD's monotone iterate; keep current
  imperative implementation.
- **`sutra_review` with commit/SHA inputs** — requires temporal support
  (v1.1). v1 supports `branch`, `staged`, `unstaged` only.
- **FCA lattice browser** — agents need implications and violations, not
  concept navigation.
- **Cross-workspace convention baselines** — per-workspace only for v1.
- **Clone detection, refactoring planner, AST pattern search** — separate
  features, not part of v1.

## Further Notes

### v1.1 roadmap

After v1 ships and is used in real review workflows, v1.1 adds:

- **Milestone 5: Temporal MVP** — associate parse snapshots with git SHAs,
  support `sutra_review(base_sha, head_sha)`, row validity for "as of"
  queries.
- **Milestone 6: HRR ranking** — re-rank `affected_symbols` and
  `recommended_reads` using HRR similarity, subsystem clustering, temporal
  analogy ("this looks like that prior change").
- **Milestone 7: MCP facade** — consolidate tools into action-oriented
  surface (`sutra_graph`, `sutra_query`, `sutra_review`, `sutra_config`,
  `sutra_help`), keep old tools as aliases.

### Spike verdicts informing this PRD

| Spike | Verdict | v1 role |
|---|---|---|
| HRR semantic/temporal (sutra/v1/1) | Viable | Deferred to v1.1 (ranking) |
| Differential dataflow (sutra/v1/2) | Viable with caveats | v1: maintained views (cycles, blast radius, forbidden deps) |
| Formal concept analysis (sutra/v1/3) | Viable with caveats | v1: convention detection and violation checking |
| Salsa (sutra/v1/4) | Skip | Not used |

### Open questions for v1.1

- Should `sutra_review` become the primary public tool, with existing tools
  treated as expert-mode primitives?
- Should DD state be persisted in a compact derived-state cache, or always
  rebuilt from SQLite?
- What is the minimum temporal schema that enables useful `base/head` review
  without committing to full historical storage?
- Should FCA conventions be per-workspace only, or should sutra maintain a
  cross-workspace baseline to filter language rules?
- How should HRR annotations enter the system: agent feedback, docs, commit
  summaries, code comments, or all of the above?

### FCA is hand-rolled, not a library

NextClosure and implication mining are implemented directly, extracted from
the spike (`src/bin/fca-spike.rs`). Reasons:

- The core algorithm is ~200 lines. A library doesn't buy much.
- Incremental updates are specific to sutra's data model (symbol attributes
  from tree-sitter parse). No off-the-shelf FCA library supports this.
- No actively maintained Rust FCA library with approximate implication support.
- The spike validated correctness against two codebases with known ground
  truth (sutra: 72 files / 651 symbols, chitta: 32 files / 300 symbols).

### Dependencies

- `differential-dataflow` and `timely-dataflow` crates (already spiked in
  `src/bin/dd-spike.rs`)
- No new external dependencies for FCA (hand-rolled)
- Remove `salsa` dependency from Cargo.toml before v1 work begins
