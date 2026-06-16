# survey: GitNexus, GritQL, SeeFlow

Three projects explored for ideas to incorporate into sutra's evolution as a
codebase understanding layer for agents and humans.

---

## GitNexus

TypeScript code intelligence tool. Builds a knowledge graph (44 node types,
21 edge types) from any codebase, exposes it via MCP, HTTP, and CLI. Uses
LadybugDB (embedded graph DB, Cypher). Main differentiator: precomputes
relational structure at index time so a single query returns complete context,
making smaller models competitive with larger ones.

PolyForm Noncommercial license.

### Execution flows ("processes") as first-class entities

After community/component detection, GitNexus runs a forward BFS from scored
entry points along CALLS edges to trace execution flows. These cut *across*
components and surface the paths code actually takes at runtime. Entry points
are scored by: few incoming callers, many outgoing calls, framework-pattern
matches, file naming conventions. Scaling: `max(20, min(300, symbolCount/10))`
processes. Each process is tagged `intra_community` vs `cross_community`.

Complementary to component grouping: components answer "what belongs together
structurally," processes answer "what happens when X runs."

### detect_changes — mapping diffs to impacted processes

Takes a git diff, intersects hunks with indexed symbol line ranges to find
affected symbols, then traces which processes pass through those symbols.
Returns a risk level. This is exactly the bridge between "what changed" and
"what might break."

### Context tool — 360-degree view of a symbol

Returns incoming edges (calls, imports, extends, implements) + outgoing edges
+ process participation + file location + field read/write access. Handles
disambiguation when multiple symbols share a name (ranked candidates with
relevance scores).

### Hybrid BM25 + vector search with Reciprocal Rank Fusion

Fuses keyword search (BM25 via FTS extension) and semantic search (384-dim
embeddings) using RRF (K=60). Additive scoring when a result appears in both
result sets. Falls back gracefully when one backend is unavailable.

### Cross-repo contract bridge

Separate bridge database connects repos that communicate via extracted
"contracts" — HTTP routes, gRPC proto definitions, Thrift interfaces, message
queue topics, database includes. Impact analysis fans out across repo
boundaries via these contracts.

### Seeded PRNG for reproducible community detection

Uses `mulberry32` with a fixed seed (`0xc0de`) for Leiden clustering.
Guarantees identical community assignments across runs — critical for
incremental-vs-full-rebuild equivalence testing.

### Shadow-mode parity harness

When migrating resolution logic (old path → new path), both paths run in
parallel and per-callsite divergences are persisted as JSON. Migration
requires ≥99% fixture parity, ≥98% corpus parity before switching. General
pattern for safely evolving any analysis pipeline.

### Incremental indexing: importer BFS expansion + shadow candidates

When files change, a bounded BFS (depth=4) over IMPORTS edges pulls in
transitive importers whose stale CALLS edges need rewriting. For newly-added
files, "shadow candidates" enumerate existing paths whose import resolution
the newcomer could steal (same basename different extension, bare file vs
directory index, etc.). These get seeded into the BFS frontier. Crash
recovery via an `incrementalInProgress` flag that forces full rebuild if set.

### Unified capture tags across tree-sitter grammars

A single vocabulary (`@definition.class`, `@call.name`, `@import.source`,
etc.) across 16+ languages, decoupling extraction logic from
language-specific branching.

### Other notable details

- 4-index KnowledgeGraph: nodeMap, relationshipMap, relationshipsByType,
  edgeIdsByNode — O(1) removal without full scans.
- Iterative C3 MRO linearization with WeakMap cache — handles 10K+ deep
  class hierarchies without stack overflow.
- SCC-ordered cross-file return-type propagation — multi-hop alias chains
  collapse in a single linear pass.
- Two-channel binding lifecycle: frozen finalize + append-only augmentation
  channel for post-finalize extensions.
- CSV streaming load into graph DB with per-stream backpressure.
- Sibling-clone drift detection across repos sharing the same git remote.

---

## GritQL

Rust structural query-and-rewrite language for code. Any code snippet in
backticks is a valid query; `$metavariables` act as holes. Precision added
via `where` clauses rather than working at the raw AST level. Uses tree-sitter
for parsing, Rayon for parallelism. Targets 10M+ line codebases.

### Snippet-as-pattern paradigm

The core idea: `` `console.log($msg)` `` is both valid JavaScript and a valid
query. Users write code to find code. The snippet is parsed by the *target
language*'s tree-sitter parser (after substituting `$var` → `µvar` so
tree-sitter sees identifiers, not syntax errors). Multiple parse contexts are
tried (expression, statement, etc.) so the snippet works in any valid
position without requiring users to know AST node types.

Multi-interpretation: one snippet can parse to multiple AST node kinds
(e.g. `foo()` as both `call_expression` and `expression_statement`). All
interpretations are stored; the correct one is dispatched at runtime by
checking the target node's `kind_id`.

### Text hoisting as a pre-filter

Before tree-sitter parsing, the optimizer extracts string literals from the
compiled pattern and does a raw text scan of each file. Only files containing
the substring get parsed. Primary performance optimization for large
codebases. Composes correctly with pattern structure (And/Or in the pattern →
And/Or in the filter).

A second optimizer hoists filename predicates (`$filename <: includes "test"`)
to skip files entirely before opening them.

### Equivalence classes for leaf nodes

Multiple distinct token types can be declared equivalent for matching. `'foo'`
and `"foo"` match each other in JavaScript; `True` and `true` in Python.
Language-specific normalizers strip syntactic noise. Reduces false negatives
from superficial style differences.

### Disregarded fields for structural matching

Each language declares AST fields that are "don't-care" for snippet matching.
JavaScript disregards `parenthesis` and empty `async` fields on function
declarations, so `` `function foo() {}` `` matches both sync and async.
Controlled structural fuzziness, configurable per-language.

### Full pattern logic language

GritQL's query expressiveness:

- Matching: code snippets, explicit AST node + field matching, metavariables,
  wildcards, sequence wildcards (`...`), regex
- Logic: and/or/any/not/maybe, some/every quantifiers
- Traversal: `contains` (deep subtree), `within` (ancestor), `before`/`after`
  (sibling position), `contains p until q` (bounded)
- Rewriting: `p => q` (replace), `p += q` (insert), `p => .` (delete)
- Composition: named patterns with parameters, predicates, functions, foreign
  functions (WASM/JS interop)
- Built-ins: string manipulation, `llm_chat`, path resolution, sort, distinct
- Multi-pass: `sequential { step1, step2 }` with file versioning
- Scope: file-level patterns, `$filename`, `$program` (root)

### Effect system for non-destructive rewrites

Rewrites accumulated as effects during matching, applied only after match
success via interval-based linearization (earliest-deadline sort). Effects can
nest. The match/mutation separation makes correctness much easier to reason
about. If sutra ever supports automated transformations, this pattern is
proven.

### Pattern hash caching

Each compiled pattern gets a SHA-256 hash. Files previously analyzed with the
same pattern hash that had zero matches are skipped on re-runs.
Content-addressed caching of negative results.

### Notebook source-map layer

Jupyter notebooks concatenated into a single pseudo-file for analysis, with a
source map for byte-offset → cell-local mapping. Transparent to the pattern
engine.

### Other notable details

- 3D variable registry `Vec<Vec<Vec<Box<VariableContent>>>>` indexed by
  `[scope_id][invocation_depth][var_index]`. Scopes statically allocated at
  compile time; invocation frames pushed/popped dynamically.
- `FilePtr { file: u16, version: u16 }` — 4-byte handle for versioned file
  references across sequential rewrites.
- Variable mirroring: binding a parameter propagates back to the caller's
  variable via a mirror chain.
- Metavariable prefix substitution: `$` → `µ` (Unicode micro sign) so
  tree-sitter parses metavar holes as identifiers.

---

## SeeFlow

TypeScript/Bun/React tool for turning static architecture diagrams into live,
interactive control panels. An AI agent reads the codebase, generates a
`seeflow.json` describing architecture as a node graph, wires each node to
real scripts, and the resulting canvas runs in a browser with SSE-based
real-time updates.

### Architecture diagrams generated from code, not drawn by hand

The core value prop: diagrams that can't drift because they're derived from
the codebase. Uses a multi-agent pipeline: discoverer → node planner → script
designers → validator. The principle applies broadly: if the component
architecture and visualization are good enough, they replace hand-drawn
architecture diagrams entirely.

### Score-based entry-point proposal

Scoring heuristic for identifying entry points: canonical names (`server.ts`,
`main.go`, `app.py`) → +10, `src/` prefix → +4, shallow path depth → up to
+6, test files → -8, `node_modules` → -50. Returns top 30 candidates.
Complementary to graph-based entry-point detection (few callers, many
callees).

### Integration tests as a knowledge source

The discoverer agent specifically finds integration/e2e tests and extracts
setup patterns, port assignments, and payload shapes — because tests already
encode how to start the app and exercise its endpoints. High-signal files for
understanding a subsystem's behavior and contracts.

### Multi-agent pipeline with strict information boundaries

The node-planner agent has NO tools — it can't read files. It receives only
the discoverer's structured brief. Forces the discoverer to extract everything
needed; prevents the planner from hallucinating based on raw file reads. The
pattern: when using LLMs, separate the "gather facts" agent from the "reason
about facts" agent.

### Floating edge geometry and parameterized perimeter pins

For graph visualization: edges slide to the perimeter intersection of a
center-to-center ray (not fixed port handles). When a user explicitly pins
an endpoint, position is stored as `{side, t}` where `t ∈ [0,1]`
parameterizes position along that side — survives node translation and resize.

### Discriminated-union outcome pattern

Every operation returns `{ kind: 'ok', data } | { kind: 'notFound' } | ...`.
Both REST and MCP layers pattern-match on `kind`. Zero duplicated business
logic between transport layers.

### Other notable details

- "No mocks, ever" — scripts must call real services, validation triggers
  real APIs. A flow with one honest gap is better than one that silently lies.
- Status scripts emit newline-delimited JSON to stdout; the runner parses
  each line and discards malformed ones.
- File watcher with per-directory reconciliation: computes desired
  directory → basenames map, set-diffs against existing watchers.
- Atomic file writes via tempfile-then-rename.
- Schema-before-commit: every mutation re-validates the whole schema before
  writing to disk.
- `validationSafe: false` flag on play actions that would charge money or
  hit third-party services.

---

## Cross-cutting themes

### Execution flows complement component grouping

Components (static structure) + processes (runtime paths) give two orthogonal
views. Components answer "what belongs together"; processes answer "what
happens when X runs." Both are needed for full understanding.

### Pre-filtering before expensive analysis

GritQL's text hoisting, GitNexus's file hash diffing, pattern hash caching —
all share a principle: do the cheapest possible check first to avoid expensive
work. Content-addressed caching of analysis results improves incremental
performance.

### Structural code querying is a gap

grep finds text. Symbol search finds names. Neither finds structural patterns
("all functions that take a context parameter and return an error," "all match
arms that don't handle the None case"). GritQL proves this is tractable and
fast at scale. Even a subset — snippet matching + metavariables — would be a
major differentiator.

### Entry points are a universal need

GitNexus, SeeFlow, and any orient/flow-tracing mode need to identify where
execution starts. Combining graph-based scoring (fan-in/fan-out) with filename
heuristics gives robust detection.

### Multi-repo awareness via contracts

Real systems span multiple repos communicating via HTTP, gRPC, queues.
GitNexus's contract bridge extracts these boundaries and enables cross-repo
impact analysis. Even lightweight contract extraction (HTTP route → handler
mapping) would be valuable.

### The graph as query surface, not just implementation

Both GitNexus and sutra build rich graphs, but GitNexus exposes the graph
directly (Cypher access, process-oriented results, 360-degree context).
Exposing structured traversal — not necessarily a query language, but more
than pre-built tools — unlocks use cases beyond what fixed tools anticipate.

### Information boundaries in LLM pipelines

SeeFlow's strict separation (discoverer gathers facts with tools, planner
reasons without tools) produces better results than giving every agent full
access. When sutra uses LLMs for enrichment, structuring the pipeline this
way avoids hallucination.

### Reproducible analysis

Seeded PRNGs for clustering, content-addressed caching, shadow-mode parity
testing — all serve the same goal: analysis results should be deterministic
and verifiable. Critical for trust in incremental updates.
