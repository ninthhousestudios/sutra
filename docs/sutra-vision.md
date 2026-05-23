# sutra vision

> **The thread that holds your codebase together.**

## Mission

Sutra exists so that human-AI teams produce coherent software, not just
functional software.

It maintains a living architectural model of the codebase -- discovered from
code, refined by human intent, enforced during development -- that gives both
humans and agents the shared understanding needed to build well together. The
architect gets confidence that what was built fits the design, without reading
every line. The agent gets architectural context that persists across sessions,
so it writes code that belongs, not just code that compiles.

## Problem statement

In human-AI collaborative development, the architecture, conventions,
constraints, and design intent of a codebase exist mostly in the human's
head -- not in a persistent, machine-readable form that both humans and agents
can consult and that the system can enforce. This creates two cascading
failures:

**1. Agents work without architectural context.** Each session starts from
scratch. The agent doesn't know the component boundaries, the naming
conventions, the dependency rules, or the design intent. It can write correct
code, but it can't write code that *fits* -- because "fitting" requires
knowledge that isn't in the code.

**2. Humans can't verify what agents build.** The human architect can read a
plan and judge whether it's sound. But once the agent implements it, verifying
that the implementation respects the architecture and actually does what was
intended requires reading every line -- which defeats the purpose of using an
agent. Without a way to verify architectural fit and behavioral correctness at
a higher level than line-by-line review, the human loses confidence in their
own codebase.

The result: code that compiles, passes tests, and may individually look fine,
but that gradually drifts from any coherent architecture -- because there's
nothing persistent and machine-readable to drift *against*.

### Root cause

The understanding gap (architect doesn't fully understand the implementation)
and the quality gap (code drifts from coherent architecture) share a root
cause: no living blueprint. Understanding enables quality -- if you understand
the architecture and code well, quality is much easier to achieve. The
blueprint isn't just documentation; it's the thing that makes quality
achievable.

### Where pain concentrates

With design process tools (vidhi) and task tracking (yojana) in place,
the planning and cross-session continuity problems are handled. The gap is
in the middle:

- **During writing** -- agents make structural decisions without architectural
  context. Helped significantly by good plans, but plans don't enforce
  themselves.
- **After writing, before merge** -- the architect can't verify that the
  implementation respects the architecture and actually does what was intended
  without reading every line.

The two things the architect most needs to verify:

1. **Architectural fit** -- does the change respect component boundaries,
   dependency directions, and structural organization?
2. **Behavioral correctness** -- does the logic actually do what was intended,
   verifiable without reading every line of implementation?

## What sutra does

**1. Discovers and maintains a living architectural model.**
Builds a persistent understanding of the codebase: its components (groups of
related code), how they relate, what conventions they follow, where the
boundaries are, where the health problems are. Primarily computed from code;
refinable by human-authored constraints and boundaries.

**2. Informs agents before they write code (Orient).**
An agent about to modify a module gets the conventions, boundaries, key
dependencies, and cautions that apply. The architectural context that no agent
remembers session-to-session is persisted and served on demand. Canonical
interactions:

- "I am about to edit X; orient me."
- "Here is my plan; check architectural fit."
- "Give me local templates and conventions for adding Y."

**3. Enforces constraints during and after writing (Guard).**
Convention deviations, boundary violations, health degradation -- flagged
actively, not just queryable. Code that breaks architectural rules gets caught
before it lands.

**4. Enables verification without line-by-line reading (Verify).**
Architectural delta of a change: "Here is my diff; review architectural
delta." The primary artifact is the architectural change report --
components touched, new dependencies, boundary crossings, conventions
followed or broken, semantic anchors changed, health deltas, and
recommended files to inspect manually. The human reads this report, not
every line of implementation. Correctness verification via orchestrated
tools (property tests, contracts, bounded model checking) enriches the
report with behavioral evidence where available.

**5. Makes the architecture legible to the human (Understand).**
Component navigation, health dashboards, convention catalogs, architectural
explanations. The thing that lets the architect who didn't write every line
actually understand the shape of what they're building.

## What sutra does not do

- **Write code.** Sutra understands, verifies, and guides. It doesn't
  generate.
- **Manage tasks or workflow.** That's yojana.
- **Run the design process.** That's vidhi.
- **Store human knowledge or memory.** That's chitta.
- **Replace the compiler, linter, or type checker.** Sutra works at the
  architectural level. cargo check and clippy handle syntax and types. Sutra
  handles fit, coherence, and intent.
- **Validate specs or requirements.** Sutra works on code that exists or is
  being written. Spec validation is upstream (vidhi + human judgment).
- **Require an LLM to function.** Core intelligence is structural --
  tree-sitter, dependency graphs, pattern analysis. LLMs enrich (better
  labels, natural-language explanations) but aren't required. Sutra works on
  a laptop with no cloud access.
- **Own why the work exists.** Sutra owns codebase facts and enforceable
  architectural state. Task intent lives in yojana; design rationale lives
  in vidhi. Sutra references those systems when needed but does not
  duplicate their data.

## Identity

Sutra is a **collaborator** -- not a passive oracle or a simple gatekeeper,
but an active participant in development. To collaborate well, it needs to
understand (oracle capability) and enforce (guardian capability), but the
point isn't to sit there passively. It actively shapes what gets built.

## Core loop

The layer model describes sutra's internal structure. The core loop describes
the daily experience of using it.

1. Human or agent selects a task.
2. **Orient** -- sutra briefs the agent on the relevant architecture:
   conventions, boundaries, key dependencies, cautions, and structural
   templates for the area being changed.
3. Agent writes code.
4. **Check** -- sutra checks the diff for architectural fit and convention
   drift as the agent works, flagging violations actively.
5. **Review** -- sutra produces an architectural change report the human can
   assess without reading every line: components touched, boundary crossings,
   convention compliance, semantic anchors changed, health deltas, verification
   evidence, and recommended files to inspect manually.
6. **Teach** -- human accepts, rejects, or refines sutra's model by updating
   constraints, component boundaries, aliases, or convention lifecycle states.
   Sutra learns from these corrections.

The orient-check-review-teach loop is the product. The layers are how it
works internally.

## Trust model

If sutra is noisy, humans and agents will route around it. Sutra needs a
trust model, not just a rule model.

**Confidence.** Every architectural claim carries provenance (see below) and a
confidence level. Computed structural facts are high confidence. Inferred
conventions are medium. Auto-learned concept mappings are low until confirmed.

**Severity.** A boundary violation, a weak convention deviation, and a health
regression are not the same kind of failure. Findings carry severity:

- **blocking** -- must be addressed before merge (boundary violations,
  explicit constraint failures)
- **advisory** -- flagged for human judgment (convention deviations, health
  regressions, inferred invariant changes)
- **informational** -- reported but not flagged (new dependencies within
  allowed boundaries, concept mapping updates)

**Waivers.** Humans can waive specific findings with rationale. Waivers are
tracked, not silent -- they appear in every review report that touches the
waived area so the architect can audit what's been accepted.

**Sketch mode.** Components in active prototyping can be marked as sketching
(see Layer 1). In sketch mode, all convention lifecycle states flatten to
informational -- conventions are tracked but not enforced. Constraints remain
fully enforced; violating a constraint during a spike may render the spike's
conclusions meaningless.

## The living architectural model

The model is organized as layers, each building on the ones below. Lower
layers are factual and computed; higher layers are richer and more
interpretive. Together they constitute the "living blueprint."

**Provenance.** Every claim in the model carries its origin:

- **computed** -- derived directly from code (Layer 0 facts, graph metrics)
- **inferred** -- statistically detected (FCA conventions, cluster boundaries)
- **human-authored** -- explicit constraints, boundaries, aliases
- **ADR-derived** -- extracted from architectural decision records
- **agent-learned** -- auto-captured from agent sessions

Discovered architecture is a proposal, not truth. Human-authored claims
override inferred ones. Provenance lets the architect audit what sutra
believes and why.

### Layer 0 -- Structural facts (ground truth)

**What it captures:** What exists in the code. Files, symbols (functions,
types, traits, modules), and relationships (calls, imports, contains,
implements). The raw skeleton of the codebase.

**Substrate:** Tree-sitter parsing into relational storage (SQLite). This is
language-agnostic -- per-language adapters extract the right AST nodes, but
the schema (files, symbols, edges) is the same regardless of language.

**Incrementality:** Tree-sitter re-parses changed files and produces a delta
(symbols added, removed, or changed). This delta feeds all layers above.

### Layer 1 -- Architecture (emergent structure)

**What it captures:** How code organizes into coherent units. Components
(groups of related code), their hierarchy, their relationships, their
boundaries. The answer to "what are the pieces of this system and how do they
relate?"

**Substrates:**
- Graph clustering (Louvain/Leiden on call/dependency graph) -- groups code
  that talks to each other
- HRR structural similarity (strip mode) -- catches structurally similar code
  that isn't connected by call edges (interface implementations, handler
  families, test helpers)
- Human input -- explicit boundaries, component names, aliases

**Incrementality:** Components are stable. They don't need to recompute on
every edit. Recompute on demand, on save, or when structural changes
exceed a threshold. Stability is a feature -- the architecture should not
churn on every keystroke.

**Component identity and lifecycle.** Components are not ephemeral clusters;
they are persistent entities with identity, history, and state.

- *Identity:* Components have stable names and IDs that survive
  recomputation. If clustering shifts a boundary, sutra tracks the change
  as a merge, split, or drift -- not a fresh set of components.
- *History:* "This component used to be X and is becoming Y" is more useful
  than a freshly computed cluster with no memory.
- *Lifecycle state:* Each component is either **sketching** (actively
  prototyping -- conventions informational, constraints still enforced)
  or **stable** (architecture locked in -- conventions and constraints both
  enforced). Default is stable; the human sets sketching explicitly.

**Ideas:**
- *Semantic anchors:* Each component has anchor points -- the central types,
  the load-bearing functions, the key abstractions. Identified by graph
  centrality (high fan-in), stability (rarely changes), and naming. If you
  understand the anchors, you understand the system. Sutra surfaces these as
  "start here" pointers for both humans and agents.
- *Concept density:* A module with 5 functions that each do something
  different is conceptually dense (hard to understand per LOC). A module with
  50 similar handlers is conceptually sparse (repetitive, easy to skim). This
  metric matters for review prioritization -- dense code needs more human
  attention.

### Layer 2 -- Conventions (implicit rules)

**What it captures:** Patterns the code actually follows. "Handlers look like
this." "Error types implement Display." "Public functions return Result."
Conventions are *detected*, not authored -- they emerge from the code.

**Substrate:** Formal Concept Analysis (FCA) on the symbol-attribute matrix.
Validated in spike (sutra/v1/3). Extract implications, filter by
support/confidence, get real conventions. Violations are symbols that break
implications.

**Convention lifecycle.** Detected conventions aren't automatically good.
Legacy code often follows the wrong pattern; FCA may detect conventions that
are accidents, stale design, or local compromises. Every convention has a
lifecycle state:

- **descriptive** -- this pattern is common (default for detected conventions)
- **preferred** -- this pattern should continue (human-promoted)
- **deprecated** -- this pattern exists but should fade (agents warned away)
- **forbidden** -- do not copy this (agents blocked, violations flagged)

Agents are oriented with preferred conventions and warned about deprecated
ones. Forbidden conventions generate violations when new code matches
them. Descriptive conventions are informational until promoted.

**Incrementality:** Check changed symbols against existing implications. FCA
can update incrementally without full recomputation.

**Ideas:**
- *Effect tracking as FCA attributes:* Enrich the attribute matrix with
  effects -- does this function touch the filesystem? Network? Database?
  Mutable state? In Rust, `unsafe` and `async` give hints. In any language,
  trace which side effects are reachable transitively. This gives FCA richer
  material: "all functions in this component are pure" becomes a detectable
  convention.
- *Structural templates:* When FCA detects a convention, extract it as a
  concrete template with metavariables. When an agent is about to write a new
  handler, sutra provides the template: "here's what handlers in this
  component look like." The collaborator identity in action -- sutra doesn't
  just detect conventions, it teaches them.

### Layer 3 -- Constraints (explicit rules)

**What it captures:** Rules that must hold. Architectural boundaries ("db must
not import http"). Design decisions ("we use event sourcing"). API stability
rules. Constraints are *authored*, not detected -- they encode human intent.

**Substrate:** Differential dataflow (DD) maintained views. Express rules as
Datalog-like views that continuously check the graph. Validated in spike
(sutra/v1/2) for cycle detection, blast radius, and forbidden dependencies.

**Incrementality:** DD's core value proposition. Feed it fact deltas from
Layer 0; all constraint views update automatically with only the affected
portion. This is what makes real-time constraint checking viable during
active development. DD powers the recursive/transitive graph analyses
(cycles, reachability, blast radius) that would be painful to incrementalize
by hand.

**Ideas:**
- *ADRs as live constraints:* Parse architectural decision records and extract
  checkable constraints. "ADR-004 says we use event sourcing" becomes a
  constraint that sutra checks: no code directly mutates the state store.
  Decisions become self-enforcing instead of forgotten.

### Layer 4 -- Health (derived quality state)

**What it captures:** Quality metrics per component and per file. Complexity,
coupling, cohesion, churn, instability, god-class scores. Trends over time.

**Substrate:** Derived from Layer 0 (structural facts) + git history. Fan-in,
fan-out, instability (Martin's Ce/(Ca+Ce)), component cohesion
(internal_edges/total_edges), cyclomatic complexity from AST, churn from git
log.

**Incrementality:** Simple metrics (fan-in/out counts) update trivially from
deltas. Transitive metrics (blast radius aggregates) update via DD. Trends
accumulate in snapshot tables.

**Ideas:**
- *Convention drift detection:* Track convention cohesion as a time series.
  If each agent session introduces slightly different patterns, the codebase
  diverges even though each individual change looks fine. Alert when variance
  within a component exceeds a threshold. This catches the specific failure
  mode of multi-agent development.

### Layer 5 -- Vocabulary (human-to-code mapping)

**What it captures:** The mapping from human concepts to code locations.
"Settings screen" maps to these files. "Auth flow" maps to this component.
"The parser" maps to this module.

**Substrates:**
- Component labels from Layer 1 (auto-computed)
- Human aliases (`.sutra/aliases.toml`)
- HRR embed-mode vectors for fuzzy concept matching ("the screen where users
  change their preferences" matches the Settings component even without a
  keyword hit)
- Optionally: auto-learned from agent sessions

**Incrementality:** Mostly static. Updates when component labels change.

### Layer 6 -- Similarity (structural relationships)

**What it captures:** Which code looks like other code. Near-duplicates.
Pattern families. Structural analogy across the codebase.

**Substrate:** HRR vectors. Strip mode for structural similarity ("functions
that work like this one"), embed mode for semantic similarity ("functions
about the same thing"). Validated in spike (4.4x lift on structural search,
57% on transform search).

**Incrementality:** Recompute vectors for changed symbols only.
Sub-millisecond per function.

**Ideas:**
- *Semantic diff via HRR:* Not "what lines changed" but "what structural
  shape changed." Two functions with different text but identical HRR vectors
  equals safe refactoring. Small text diff but large HRR change equals subtle
  behavioral change -- flag it. Gives the architect a much higher-level view
  of what changed.

### Layer 7 -- Verification (behavioral correctness)

**What it captures:** Evidence that code does what it's supposed to, without
the human reading the implementation.

**Deferral note:** This layer is the most exploratory and the most expensive
to make broadly useful. The core sutra identity is the orient-check-review
loop (Layers 0-4). Layer 7 enriches that loop with behavioral evidence but
is not required for it. Build the architectural orient/review system first;
add verification as the foundation matures.

**Substrates (orchestrated, per-language):**
- Property-based testing (proptest, hypothesis) -- "for all inputs satisfying
  X, output satisfies Y"
- Bounded model checking (Kani, CBMC) -- proves absence of panics, overflows
  for all inputs
- Mutation testing (cargo-mutants, mutmut) -- "do the tests actually test
  anything?"
- Behavioral contracts (`#[pre]`/`#[post]`) -- human reads the contract, not
  the implementation

**Incrementality:** On-demand, not real-time. Triggered at review time, CI
time, or explicit request.

**Verification gaps.** For human trust, sutra must be explicit about what was
*not* verified, not just what passed. The review report includes:

- no contract exists for this behavior
- mutation score is weak in this area
- this change touched high-risk logic with no behavioral evidence
- this property was inferred but not human-approved

Negative evidence and missing evidence are first-class findings. The human
should never mistake silence for safety.

**Ideas:**
- *Invariant mining:* Daikon-style automatic inference from existing code and
  test executions. Infer likely invariants ("this function always returns a
  positive number," "this field is never None after init"). Check new code
  against inferred invariants. The human reviews invariants (high-level,
  readable) not implementation (low-level).
- *Intent-to-property extraction:* When a task says "add rate limiting,"
  extract testable properties: "no client exceeds N requests per window,"
  "rate-limited requests get 429." These become verification targets. If the
  properties are right and verification passes, the implementation is
  constrained to be correct.

### Real-time update flow

When code changes during a session, this is the propagation path:

```
File edited
  -> tree-sitter re-parses (Layer 0 delta)
  -> DD ingests delta, graph views update (Layer 3 constraints, Layer 4 health)
  -> HRR recomputes vectors for changed symbols (Layer 6 similarity)
  -> FCA checks changed symbols against conventions (Layer 2 conventions)
  -> Sutra can immediately answer:
       "Did this change introduce a cycle?"
       "Did it violate a boundary?"
       "Did it break a convention?"
       "Did it increase blast radius?"
```

### Computational substrates summary

| Substrate | Layers powered | Validated? |
|---|---|---|
| Tree-sitter | 0 (parsing) | Yes (current sutra) |
| SQLite | 0 (persistence), all (storage) | Yes (current sutra) |
| DD (differential dataflow) | 3 (constraints), 4 (transitive health) | Spike: viable with caveats |
| FCA (formal concept analysis) | 2 (conventions) | Spike: viable with caveats |
| HRR (holographic reduced representations) | 1 (clustering), 5 (vocabulary), 6 (similarity) | Spike: viable |
| Graph clustering (Louvain/Leiden) | 1 (architecture) | Not yet spiked |
| Per-language verification tools | 7 (verification) | Not yet spiked |

## Design decisions (resolved)

**Language scope: language-agnostic core from day one.** The core model
(components, conventions, constraints, verification results) is
language-independent. Per-language support via adapters/plugins for parsing,
convention detection, and verification tool integration. Tree-sitter already
provides multi-language parsing. Target languages: Rust, Dart, Python, C.

Language-agnostic does not mean uniform. The semantic richness of languages
varies enormously -- Rust gives rich type-level information, explicit traits,
and strict visibility; Python's call graphs and effects are fragile to
analyze statically. Each adapter declares **capability levels**: which facts
it can produce, which analyses it supports, and with what confidence. The
core adapts gracefully -- a language with no effect tracking simply produces
fewer convention attributes, not wrong ones.

**Verification tool orchestration: sutra owns the pipeline.** Sutra knows
which verification tools apply to which language, when to run them, and how
to parse their output into a common architectural format. The orchestration
layer is language-agnostic. Tool adapters are thin, per-language plugins.
Example tools by language:

| Language | Verification tools |
|---|---|
| Rust | Kani (bounded model checking), MIRI (UB), proptest, cargo-mutants |
| Python | hypothesis (property testing), mypy, mutmut |
| C | CBMC (model checking), AFL/libfuzzer (fuzzing), Valgrind/ASAN |
| Dart | analyzer, test framework (thinner ecosystem) |

**Concept persistence: sutra owns the codebase vocabulary.** The mapping from
human concepts ("settings screen") to code locations (`src/ui/settings.rs`)
is a fact about the codebase, not about the human (that's chitta's domain).
Sutra persists these mappings so agents don't rediscover them every session.
Sources: component labels (auto-computed), human aliases (explicit), and
optionally auto-learned from agent sessions.

**Zero-config default.** Sutra must extract high value from Layer 0 alone,
with no configuration. Aliases, constraints, convention promotions, and
component boundaries enrich the model, but a fresh `sutra init` on an
unknown codebase should immediately produce useful orientation and review.
Configuration is refinement, not setup cost.

**UI: sutra serves data via API; UI is a separate concern.** Sutra exposes
the architectural model, verification results, and navigation data through a
structured API. Whether the UI is an embedded web server, a separate desktop
app, or both is an implementation decision. The API must exist regardless.

## Open design questions

- Should concept mappings auto-learn from agent sessions, or only accept
  explicit human aliases?
- What is the adapter interface for adding a new language? Tree-sitter
  grammars handle parsing, but what about language-specific FCA attributes,
  verification tools, convention detection, and capability level declarations?
- Where does the boundary fall between sutra's verification orchestration
  and CI's job?
- Graph clustering (Louvain/Leiden) for Layer 1 needs a spike. How does it
  perform on real codebases? How stable are components across changes? How
  does component identity survive recomputation?
- How should structural templates (from Layer 2) be represented and served
  to agents?
- What is the right DD scope -- just constraints and transitive health, or
  should more analyses migrate into DD?
- What is the right granularity for waiver tracking? Per-finding,
  per-file, per-component?

## Design process

This project is too large for a single PRD-to-implementation cycle. It
decomposes into phases, each producing focused artifacts.

### Phase 1 -- Vision (complete)

This document. Problem, mission, model, layers, decisions.

### Phase 2 -- Domain

One vidhi-domain session to sharpen vocabulary across all layers. Terms like
"component," "convention," "constraint," "verification" need precise
definitions -- they'll appear in every PRD, API, and tool name.

### Phase 3 -- Architecture

Brainstorm on how sutra is structured as software (distinct from what it
does). Key questions: one binary or multiple? Storage model? Language adapter
interface? API surface that all layers expose through? This cuts across every
capability and must be decided before subsystem PRDs.

### Phase 4 -- Spike: graph clustering

Layer 1 (architecture/components) depends on graph clustering
(Louvain/Leiden), which hasn't been validated yet. Spike it on real codebases
before designing the capability around it.

### Phase 5 -- Capability PRDs (in dependency order)

Each subsystem gets its own brainstorm, PRD, decompose, implement cycle:

```
a. Core model (L0 redesign, storage, adapter interface)
   |
   v
b. Component discovery (L1 + L5 vocabulary)
   |                         |
   v                         v
c. Convention system (L2)    d. Constraint system (L3 + DD)
   |                         |
   v                         v
e. Health + similarity (L4 + L6)
   |
   v
f. Verification orchestration (L7)
```

c and d are independent of each other -- they can be designed in parallel.
f (verification) is the most independent and most exploratory -- could be
designed anytime, but benefits from the rest being solid.

Implementation begins after the first capability PRD (core model), not after
all design is complete. Each PRD decomposes into yojana vertical slices.

## Etymology

Sutra (Sanskrit: sutra) means "thread" -- the thread that connects ideas.
Sutra is the thread connecting intent to architecture to code to verification.
The thread that holds a codebase together.
