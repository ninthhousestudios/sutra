# Codex v1 PRD Ideas

Context: comparison of Sutra with `better-code-review-graph`, adjusted after
the v1 spikes:

- `sutra/v1/1`: HRR semantic and temporal binding
- `sutra/v1/2`: differential dataflow for structured reasoning
- `sutra/v1/3`: formal concept analysis for convention detection
- `sutra/v1/4`: Salsa incremental computation boundary-finding

## Executive summary

Sutra should not try to become a Rust clone of `better-code-review-graph`.
Sutra already has a cleaner internal shape: smaller modules, explicit
workspace state, a local-first daemon, freshness envelopes, and more disciplined
tool implementations.

The useful lesson from `better-code-review-graph` is product shape, not
architecture. It has a clearer agent-facing workflow: a few action-oriented
tools, an explicit review context tool, runtime help, richer query diagnostics,
and a mature migration story.

The v1 spikes change the priority order. Sutra is not merely a structural code
index; it is becoming a local code intelligence substrate:

- DD should power maintained graph views and temporal reasoning.
- HRR should provide associative ranking while keeping structural facts primary.
- FCA should detect project conventions and review-time violations.
- SQLite plus content hashing should remain the persistence and parse-skip
  backbone; Salsa should be skipped for now.

## Product direction

### 1. Build `sutra_review` as the main v1 workflow

`better-code-review-graph` gets one important thing right: agents need a
single review-context entry point. Sutra currently has the ingredients
(`diff_impact`, `pr_risk`, `trace`, `hotspots`, `file_health`, `winnow`,
`cochange`), but they are exposed as separate tools.

Add a composite review workflow that answers:

- What changed?
- Which symbols changed?
- Which files and symbols are likely affected?
- What is structurally risky?
- What conventions are violated?
- Which tests or test areas are likely relevant?
- What snippets should the agent read first?
- What is the final risk verdict, with reasons?

The v1 version should be structural-first:

- Use git diff to identify changed files and changed symbols.
- Use current SQLite graph for direct references, call graph, deps, complexity,
  hotspots, and co-change.
- Use DD-maintained views where available for live impact, cycles, forbidden
  dependencies, and rollups.
- Use FCA convention violations as review findings.
- Use HRR only to re-rank and group the result set, not to invent facts.

The output should be bounded and explicit:

- `changed_files`
- `changed_symbols`
- `affected_files`
- `affected_symbols`
- `convention_violations`
- `risk_metrics`
- `verdict`
- `verdict_reasons`
- `recommended_reads`
- `freshness`

### 2. Make DD the maintained-view engine

The DD spike verdict was "viable with caveats". The product implication is
clear: use DD where automatic tuple-level incrementality matters, and avoid it
for one-shot parameterized queries where SQL or imperative traversal is simpler.

Use DD for:

- fan-in / out-degree rollups
- blast-radius rollups
- dependency cycle detection
- forbidden dependency enforcement
- file-level and symbol-level reachability views
- temporal state across epochs
- eventually, incremental co-change as new commits arrive

Do not use DD for:

- PageRank
- small one-shot root queries
- query surfaces where extracting a DD result costs more complexity than the
  traversal itself

This gives Sutra a principled split:

- SQLite persists facts and snapshots.
- DD maintains live derived graph views.
- Imperative Rust handles ad hoc query parameters.

### 3. Treat temporal reasoning as core v1 scope

`better-code-review-graph` uses temporal columns (`valid_from_sha`,
`valid_to_sha`) to query historical graph state. The DD spike shows Sutra can
approach this more naturally with epochs and maintained views.

V1 should support a practical temporal model:

- associate parse snapshots with optional git SHAs
- keep enough row validity metadata to answer "as of" questions
- support diff-native queries without forcing a full reparse
- expose temporal context through `sutra_review` and `sutra_query`

Candidate API:

- `sutra_review(base, head)`
- `sutra_query(action="diff", from_sha, to_sha)`
- `sutra_query(action="impact", as_of, target)`

Implementation should start simple. A full persistent event log can come later,
but v1 should avoid designing itself into a latest-only index.

### 4. Add FCA-backed project conventions

The FCA spike found that symbol-level contexts are useful and file-level
contexts need better attributes. V1 should productize the useful slice:

- extract approximate symbol-level implications
- filter language-rule implications where possible
- persist the resulting convention set
- check changed symbols/files against active conventions
- surface violations in review/risk output

Do not build a lattice browser. The lattice is an internal representation.
Agents need implications and violations.

Candidate API:

- `sutra_conventions(workspace, action="list")`
- `sutra_conventions(workspace, action="check", paths=[...])`
- or fold this into `sutra_review` first and split later only if needed

Examples of useful findings:

- test files with no test functions
- modules that usually expose a tool args struct but do not
- visibility patterns that differ from the local module convention
- directory-specific naming or signature conventions

### 5. Use HRR for associative ranking, not factual authority

The HRR spike indicates HRR is viable as Sutra's associative knowledge
substrate. This should change how Sutra thinks about "semantic search".

Do not prioritize generic embedding search as the core semantic layer. Instead:

- encode structural roles and semantic/agent annotations in HRR
- keep semantic bindings lower weight than structural bindings
- use subsystem-based context for impact re-ranking
- let HRR rank, cluster, and retrieve candidates
- always ground final answers in SQLite/DD facts

Good first use cases:

- re-rank `sutra_impact` and `sutra_review` affected symbols
- cluster changed files by subsystem
- retrieve related prior annotations or provenance records
- surface "this looks like that prior change" analogies

Bad first use cases:

- replacing exact symbol lookup
- replacing ref/call resolution
- making review findings without structural evidence

### 6. Improve query diagnostics

`better-code-review-graph` has a strong pattern for not-found and ambiguity
responses. Sutra should adopt this across lookup tools.

Every symbol-oriented query should distinguish:

- `no_such_symbol`
- `ambiguous_unqualified`
- `symbol_exists_but_unresolved`
- `symbol_exists_with_no_results`
- `index_stale`
- `analysis_tier_disabled`
- `partial_resolution`

Responses should include:

- candidate qualified names
- indexed kinds
- matching files
- suggested next query
- freshness envelope

This is high leverage for agents. It reduces fallback grepping and prevents
false conclusions from empty results.

### 7. Add runtime help and recipes

`better-code-review-graph` exposes package docs through a `help` tool. Sutra
should do the same, but keep it compact.

Candidate topics:

- `quickstart`
- `workspaces`
- `query`
- `review`
- `freshness`
- `conventions`
- `temporal`
- `troubleshooting`
- `recipes`

Recipes matter more than reference docs for agents. Useful examples:

- "review my current diff"
- "find callers and affected tests for this function"
- "explain why a result is stale"
- "check whether this change violates local conventions"
- "trace a path between two symbols"

### 8. Collapse the MCP facade later, not first

The earlier comparison suggested reducing many small tools into a few
action-oriented tools. That remains good UX, but the spikes make it less urgent
than the reasoning engine.

Do not churn the current MCP surface before v1 behavior stabilizes. Instead:

1. Build `sutra_review`.
2. Add `sutra_help`.
3. Improve diagnostics on existing tools.
4. Once stable, add facade tools:
   - `sutra_graph`
   - `sutra_query`
   - `sutra_review`
   - `sutra_config`
   - `sutra_help`
5. Keep old tools as compatibility aliases until the agent configs migrate.

### 9. Strengthen migration discipline before temporal schema work

Sutra's embedded migrations are fine for early development, but temporal
history and convention storage will make schema changes more expensive.

Before adding major temporal persistence, introduce:

- `schema_migrations` table
- ordered migration IDs
- checksum or content hash per migration
- clear migration failure errors
- old-DB fixture tests
- optional pre-breaking backup for local index DBs

This does not require Alembic-style machinery. A small Rust-native migration
runner is enough.

## Non-goals

### Do not copy the giant dispatcher module pattern

`better-code-review-graph` has useful product ideas, but its large dispatcher
files are not a model for Sutra. Sutra should keep:

- one module per tool or tool family
- one parser module per language
- explicit DB and pipeline boundaries
- small reviewable units

### Do not add Salsa for v1

The Salsa spike verdict was "skip". SQLite content hashes already solve
file-level parse skipping, and Salsa does not help enough with cross-file
aggregation. Revisit only if Sutra becomes an IDE-like live editing service.

### Do not make cloud embeddings required

Sutra's local-first design is a strength. Optional summaries or embeddings can
exist later, but v1 should not depend on cloud setup, credentials, or external
model availability.

### Do not build a full FCA lattice browser

Persist conventions and violations, not the lattice UI. Agents need actionable
review facts, not concept navigation.

## Suggested v1 milestones

### Milestone 1: Review workflow

- Add `sutra_review` composite tool.
- Use existing diff, graph, complexity, hotspot, and co-change data.
- Return bounded review context with verdict and reasons.
- Add tests for output shape, truncation, and stale index behavior.

### Milestone 2: Diagnostics and help

- Add structured not-found/ambiguous diagnostics.
- Add `sutra_help` with recipes.
- Add tests for empty result vs absent symbol vs ambiguous symbol.

### Milestone 3: DD maintained views

- Wire DD for fan-in/out-degree, blast radius, cycles, and forbidden deps.
- Keep SQLite as persistence source of truth.
- Add benchmark/regression tests for single-file incremental update.

### Milestone 4: Conventions

- Implement symbol-level FCA convention extraction.
- Persist active conventions.
- Check changed files/symbols.
- Surface violations in `sutra_review`.

### Milestone 5: Temporal MVP

- Associate snapshots with git SHAs.
- Add row validity or equivalent snapshot mapping for changed entities.
- Support `review(base, head)` and `query(action="diff")`.
- Avoid full event-sourcing until the API proves itself.

### Milestone 6: HRR ranking

- Add HRR-ranked impact/review ordering.
- Keep structural ranking visible for comparison.
- Add qualitative tests/fixtures for subsystem-sensitive ranking.

### Milestone 7: MCP facade cleanup

- Add action-oriented facade tools once behavior stabilizes.
- Keep old tools as aliases.
- Update README and agent recipes.

## Open questions

- Should `sutra_review` become the primary public tool, with existing tools
  treated as expert-mode primitives?
- Should DD state be rebuilt from SQLite on daemon startup, or persisted in a
  compact derived-state cache?
- What is the minimum temporal schema that enables useful `base/head` review
  without committing to full historical storage?
- Should FCA conventions be per-workspace only, or should Sutra maintain a
  cross-workspace baseline to filter language rules?
- How should HRR annotations enter the system: agent feedback, docs, commit
  summaries, code comments, or all of the above?

