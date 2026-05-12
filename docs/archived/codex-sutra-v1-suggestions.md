# Codex Suggestions for Sutra v1

Sutra currently has a strong code-intelligence substrate: it parses Rust and
Dart, stores files, symbols, references, imports, snapshots, complexity,
PageRank, freshness, cochange, impact, trace, provenance, and risk signals.
That is already useful for helping agents read code.

The larger opportunity is to make Sutra a software-understanding substrate for
agents: a system that helps agents analyze behavior, investigate bugs, reason
about architecture, plan changes, and preserve repo-specific understanding over
time.

The core shift is:

> Sutra should stop being only a code reader and become an evidence-backed
> working model of a software system.

This document summarizes a proposed direction and implementation order.

## North Star

The strongest version of Sutra is:

> A system that lets agents maintain a continuously improving,
> evidence-backed understanding of a software project, so they can debug,
> plan, modify, and architect with context comparable to a long-tenured
> engineer.

That means Sutra should help answer:

- What is this software?
- How does it behave?
- Why is it shaped this way?
- Where is it fragile?
- How should it change?
- How can we verify that change?
- What should the next agent know before touching it?

The user of Sutra is not primarily a human browsing code. The user is an agent
trying to act well inside a codebase. That changes the shape of the product.
The most valuable output is not prose documentation; it is compact, grounded,
task-specific working context.

## Current Position

As of `main`, Sutra already has the lower-level pieces needed for a larger
system:

- Tree-sitter parsing for Rust and Dart.
- A SQLite index of files, symbols, references, imports, and snapshots.
- Resolver support for local/module/direct-import references.
- File dependency graph, fan-in, blast radius, and PageRank.
- Complexity, file health, hotspots, dead-code detection, and trends.
- Diff impact, PR risk, cochange, provenance, calls, refs, and trace tools.
- Freshness envelopes and daemon/watch infrastructure.
- A first version of `sutra_winnow` for multi-axis composite query.

The current system is strongest at exact or mostly exact structural questions:

- Where is this symbol?
- Who calls this?
- What files depend on this?
- What changed together?
- What does this branch appear to risk?
- What code is stale, dead, complex, or central?

The gap is not one missing tool. The gap is a durable analysis model that can
combine these signals into higher-level software understanding.

## Recommended Implementation Order

### 1. Define Sutra's Core Analysis Object: Evidence-Backed Facts

Before adding more high-level tools, Sutra needs a common representation for
things it knows, infers, and suspects.

A fact should distinguish:

- exact structural knowledge
- inferred higher-level knowledge
- weak hypotheses
- stale or invalidated conclusions

Suggested shape:

```text
subject: entity id
predicate: relation or property
object: entity id or scalar value
evidence: source rows, file spans, commits, tool outputs
confidence: exact | high | medium | low, or numeric
freshness: current | stale | invalidated
producer: parser | resolver | git | hrr | heuristic | agent
timestamp: when derived
```

Examples:

```text
symbol:A calls symbol:B
file:X imports file:Y
file:X changes_with file:Y
module:billing owns concept:invoice
function:F mutates state:session
test:T verifies behavior:B
module:ui violates boundary:domain
symbol:S has_risk high_complexity
flow:login resembles bug_pattern:stale_session
```

This is the most important foundation. Without it, every new feature becomes a
bespoke query. With it, architecture, bug analysis, planning, concepts, and HRR
can all write into and read from the same model.

### 2. Promote Existing Signals Into Relations

Sutra should first turn what it already knows into reusable relation sets.

Existing data maps naturally into facts:

- `refs` produce `symbol calls symbol`, `symbol references symbol`, and
  `file references symbol`.
- `imports` produce `file depends_on file`.
- `symbols` produce `file defines symbol` and `symbol contains symbol`.
- `trace` produces `symbol reachable_from entrypoint`.
- `cochange` produces `file changes_with file`.
- `PageRank` produces `file centrality` and `symbol centrality`.
- `hotspots` produce `file has_risk hotspot`.
- `file_health` produces `file has_quality_score`.
- `provenance` produces `symbol changed_by commit`.
- `pr_risk` produces `diff has_risk ...`.

This is mostly consolidation, not new intelligence. The win is that later tools
can compose facts instead of reimplementing data gathering.

### 3. Add An Internal Analysis Algebra

Sutra needs reusable operations over shaped collections of facts.

This is where APL-style thinking is useful. Treat the repo as structured
relations, matrices, and vectors:

```text
files x symbols
symbols x symbols
files x commits
modules x concepts
functions x effects
tests x behaviors
modules x modules
concepts x roles
```

Then useful questions become reusable transformations:

```text
compose(symbol -> file, file -> cochanged_file)
intersect(changed_files, high_blast_radius_files)
rank(files, churn * complexity * centrality)
diff(actual_dependency_matrix, intended_boundary_matrix)
project(symbols -> concepts -> tests)
cluster(files by cochange + imports)
```

The goal is not necessarily to expose an APL-like language publicly. The goal
is to make Sutra internally capable of whole-system operations rather than
local snippet retrieval.

This also gives a natural home for optimized sparse matrices, columnar tables,
and dense HRR vectors later.

### 4. Introduce Concepts And Roles

To move from code structure to software understanding, Sutra needs concepts.

Initial entity types could include:

- file
- symbol
- module
- entry point
- test
- concept
- state
- behavior
- effect
- external system
- architectural zone
- flow
- bug pattern

Initial role/predicate vocabulary could include:

- owns
- calls
- contains
- depends_on
- reads
- writes
- mutates
- validates
- produces
- consumes
- adapts
- verifies
- guards
- violates
- resembles
- changed_with

This is where Panini's grammar is a useful analogy. The lesson is not to use
Sanskrit; it is that a small, precise set of generative rules can describe a
large structured system compactly.

Sutra could have a grammar of software facts:

```text
Module owns Concept
Function transforms Input into Output
Handler receives Event and invokes Command
Test asserts Behavior
Migration changes Schema
Boundary forbids Dependency
Invariant guarded_by Check
State mutated_by Function
```

That is more useful to agents than loosely generated prose.

### 5. Productize HRR As A Sidecar Vector Memory

The HRR spike belongs in Sutra as a representation layer, not as the product
itself and not as a replacement for exact program analysis.

Exact facts should remain authoritative:

- symbols
- refs
- imports
- call graph
- tests
- git history
- dependency edges

HRR should help with fuzzy, structural, analogical, and role-sensitive
reasoning:

- "What else has this shape?"
- "Where does this concept play the same role?"
- "What subsystem has a similar responsibility pattern?"
- "What is the likely missing relation?"
- "Where have we seen this bug pattern before?"
- "What would this code look like after applying a transformation seen
  elsewhere?"

Good initial HRR-backed entity levels:

- function-body shape vectors
- symbol vectors
- module responsibility vectors
- flow vectors
- concept-role vectors

Good first HRR tools:

```text
sutra_similar_shape(symbol, level=function|module|flow)
sutra_role_search(concept, role)
sutra_analogy(example_before, example_after, query)
```

HRR outputs should always include evidence from exact facts and source spans.
The agent should see why a result was returned, not just a similarity score.

### 6. Build Agent-Native Change Planning

The first high-level workflow should probably be change planning, because
`main` already contains most of the needed substrate: diff impact, calls, refs,
trace, cochange, risk, hotspots, file health, provenance, and freshness.

Possible tool:

```text
sutra_plan_change({
  workspace,
  goal,
  touched_or_likely_symbols?,
  changed_files?,
  base?,
  head?
})
```

Expected output:

```text
current behavior model
affected modules
likely integration points
risky files
relevant tests
architectural concerns
recommended read order
implementation slices
verification plan
evidence
uncertainty
```

This would move Sutra from "lookup tool" to "planning partner." It would also
provide immediate practical value for coding agents.

### 7. Add Bug Investigation

Bug investigation should come after the fact model and planning workflow,
because it needs the same composition machinery.

Possible tool:

```text
sutra_investigate_bug({
  workspace,
  symptom,
  stack_trace?,
  logs?,
  recent_diff?,
  failing_test?
})
```

Expected output:

```text
likely runtime paths
implicated state/concepts
suspect files ranked by evidence
recent relevant changes
similar historical patterns
reproduction candidates
tests to run or add
confidence and contradictions
```

This workflow should combine:

- stack trace symbols
- trace/call graph
- changed files
- cochange
- complexity/hotspot risk
- state/effect facts
- provenance
- similar HRR shape or bug-pattern matches

The important design principle is that every suspect should be evidence-backed.

### 8. Add Architecture And Boundary Analysis

Architecture should be modeled as actual structure versus intended structure.

The simplest first version is explicit boundaries:

```toml
[zones.domain]
paths = ["src/domain/**"]
may_import = []

[zones.api]
paths = ["src/api/**"]
may_import = ["domain"]

[zones.persistence]
paths = ["src/db/**"]
may_import = ["domain"]
```

Then Sutra can compute:

```text
actual dependency matrix - allowed dependency matrix = violations
```

Later, Sutra can infer candidate boundaries from:

- import clusters
- cochange clusters
- concept ownership
- PageRank/centrality
- HRR module responsibility similarity

Architecture outputs should distinguish:

- intended architecture
- actual architecture
- violations
- exceptions
- uncertain inferred boundaries
- architectural drift over time

### 9. Produce Context Packets

Every high-level workflow should eventually produce an agent-facing context
packet.

A context packet is not a README. It is a compact operating brief:

```text
task
current model
facts to rely on
files to read first
symbols likely involved
risks
tests to run
terms to use
known exceptions
open questions
evidence
freshness
```

Different packet types:

- module dossier
- bug investigation packet
- change planning packet
- architecture packet
- test strategy packet
- handoff packet

This should become one of Sutra's primary agent-facing surfaces.

## Conceptual Foundations

### APL

APL is useful as a design discipline for whole-system transformations.

Sutra should not only traverse trees and graphs. It should also treat the repo
as arrays of relations:

```text
files x symbols
symbols x symbols
tests x behaviors
commits x files
modules x concepts
functions x effects
```

Many useful analyses are then matrix or relation operations:

```text
change_coupling = transpose(commit_file_matrix) x commit_file_matrix
architectural_violation = dependency_matrix AND forbidden_layer_matrix
hotspot = churn x centrality x low_test_coverage
relevant_tests = changed_symbols -> concepts -> tests
```

APL's real lesson is: make shape central, operate on whole structures, and
compose small powerful operators.

### Panini

Panini suggests a compact generative grammar for software facts.

Instead of endless prose, Sutra can encode a project's structure as rules,
relations, and exceptions:

```text
Auth owns Session
Auth emits SessionCreated
Session is invalidated_by Logout
Session table may_be_written_by Auth
Exception: AdminRepairJob writes Session table
```

This is compact, durable, and agent-actionable.

### R / Statistics

Sutra should treat software as an empirical system, not only a static
structure.

Useful measurements:

- churn
- hotspots
- change coupling
- defect likelihood
- staleness
- confidence scoring
- clustering
- anomaly detection
- regression between signals and bug history

Architecture is not only the shape of code. It is shape under change.

### Datalog / Relational Algebra

Datalog is probably one of the strongest foundations for explainable inference.

Sutra facts naturally become rules:

```text
calls(f, g)
imports(a, b)
owns(module, concept)
writes(function, table)
depends_on(a, b)

risky_boundary_crossing(A, B) :-
  imports(A, B),
  layer(A, higher),
  layer(B, lower),
  forbidden_dependency(A, B).
```

This pairs well with HRR:

- Datalog for exact, explainable derived facts.
- HRR for fuzzy, analogical, role-sensitive retrieval.

### Program Slicing

Bug work needs slicing:

- backward slice: what affects this value?
- forward slice: what does this value affect?

Sutra should eventually slice across:

- code
- config
- database schemas
- events
- tests
- docs
- git history

### Abstract Interpretation

Sutra can produce conservative summaries without executing the program:

- may write DB
- may throw
- may call network
- may mutate global state
- may depend on env var
- may return null
- requires auth context

These summaries are architecturally valuable and useful for bug investigation.

### Design Structure Matrix

DSM thinking is useful for architecture:

- dependency matrices
- clustering
- cycles
- layering violations
- modularity
- parallel work planning
- change impact

This is a natural bridge between APL-style array operations and software
architecture.

### Distributed Cognition

Sutra is part of an agent's cognition. It should optimize for:

- working memory limits
- progressive disclosure
- provenance
- uncertainty
- compact context packets
- handoff
- avoiding context pollution

The question is not only "what does Sutra know?" The question is "what does an
agent need to know right now to act well?"

## Suggested First Design Document

The next design artifact should be:

```text
Sutra Analysis Facts v1
```

It should define:

- entity ids
- fact schema
- evidence schema
- confidence model
- freshness and invalidation
- analysis run tracking
- relation composition API
- how existing tools emit facts
- how HRR vectors attach to entities and facts
- how high-level tools consume facts

Suggested initial tables:

```text
entities
facts
fact_evidence
analysis_runs
entity_vectors
```

Suggested first fact producers:

- parser/resolver: definitions, containment, refs, calls
- imports: file dependencies
- graph: centrality, fan-in, blast radius
- git: cochange, provenance, churn
- analysis tools: hotspots, health, PR risk

Suggested first consumer:

```text
sutra_plan_change
```

That gives the larger vision a spine before adding many more individual tools.

## Near-Term Code Slices

If implementing incrementally, a reasonable order is:

1. Add `entities`, `facts`, `fact_evidence`, and `analysis_runs` tables.
2. Populate exact facts from existing parse data.
3. Add a small internal relation-composition module.
4. Add `sutra_context_packet` or `sutra_plan_change` using only exact facts.
5. Integrate HRR as an optional sidecar for `similar_shape`.
6. Add concept and role extraction heuristics.
7. Add explicit architectural boundaries.
8. Add bug investigation.
9. Add inferred architecture and drift detection.

Each slice should produce agent-visible value and preserve evidence links.

## Design Principles

### Exact Before Fuzzy

Use exact program facts wherever possible. Use HRR and heuristics for structural
similarity, analogy, and uncertain inference. Do not let vector similarity
silently masquerade as truth.

### Evidence Everywhere

Every high-level answer should say:

- what Sutra believes
- why it believes it
- what evidence supports it
- what evidence contradicts it
- how confident it is
- how stale it may be

### Preserve Uncertainty

Sutra should distinguish:

- known
- inferred
- suspected
- contradicted
- stale

Agents need this distinction to act safely.

### Optimize For Agent Workflows

The primary product surface should be workflows, not raw lookup:

- plan a change
- investigate a bug
- explain a subsystem
- review architecture
- find tests
- package handoff
- assess risk

Lookup tools remain important, but they are substrate.

### Compose, Do Not Duplicate

High-level tools should compose facts and relations. They should not each
reimplement file iteration, graph traversal, churn lookup, freshness checking,
or ranking.

### Keep HRR Attached To Structure

HRR is most valuable when bound to structured facts:

```text
role * filler
caller * A + callee * B
module * Billing + responsibility * InvoiceCreation
state * Session + mutation_site * AuthCallback
```

It should augment the exact graph, not replace it.

## Summary

Sutra's next version should be built around three intertwined
representations:

1. Exact symbolic graph: files, symbols, refs, imports, calls, tests, configs,
   git facts.
2. Statistical/empirical model: churn, cochange, hotspots, centrality,
   confidence, staleness, risk.
3. HRR relational memory: role-sensitive vectors for structure, analogy,
   concept-role retrieval, and shape search.

The key architectural move is to introduce an evidence-backed fact layer and
an internal relation algebra. Once those exist, change planning, bug
investigation, architecture review, context packets, and HRR-backed analogy all
become coherent extensions of the same system rather than unrelated tools.

That is the path from "help agents read code" to "help agents understand and
work on software."
