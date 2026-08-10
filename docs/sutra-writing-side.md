# sutra writing side — preventing slop in agent-written code

> **Status note (2026-08):** Planning/vision document, not a canonical spec.
> Since it was written, `sutra_orient` and the review-time FCA deviation report
> were removed (sutra/312, sutra/313) after live use showed a high false-positive
> rate; convention detection remains but is now list-only via `sutra_conventions`.
> "Orient mode" references below describe an earlier design direction that may be
> revisited, not shipped behavior.

## The problem

Agents write code. Humans direct and review. Both are responsible for
correctness. Sutra today is oriented toward *reading* — understanding what's
there. This doc explores the *writing* side: how sutra can help ensure that
code being written (by agents or humans) is correct, consistent, and
well-implemented.

The reading side and writing side share a foundation — deep understanding of
the codebase's structure, conventions, and invariants. The difference is
when and how that understanding is applied:

| Side | When | Action |
|------|------|--------|
| Reading | After code exists | Describe, navigate, explain |
| Writing | Before/during code creation | Constrain, validate, enforce |
| Review | After code is proposed | Verify, catch, flag |

Review (sutra's v1 focus) is the last line of defense. The writing side
moves quality assurance upstream — the earlier you catch a problem, the
cheaper it is.

## Three layers

### Layer 1 — Structural convention enforcement

**Source**: GritQL survey, sutra's existing FCA conventions.

Sutra already detects conventions via FCA and HRR structural similarity. The
gap: conventions are *described* but not *checked*. Adding structural pattern
matching closes this gap.

**What this looks like in practice:**

Sutra learns (via FCA or explicit rules) that handlers in this codebase look
like:

```
fn $name($ctx: &Context, $req: $req_type) -> Result<$resp_type, AppError> {
    ...
}
```

When an agent writes a new handler that takes `&str` instead of `&Context`,
or returns `anyhow::Error` instead of `AppError`, sutra flags it before the
code is committed — not as a lint error, but as a convention deviation with
the evidence: "37 other handlers in the Handlers component follow this
pattern."

**Implementation options:**

- Integrate GritQL as a library (Rust, same as sutra). Full pattern language,
  proven at scale. Heavyweight but maximally expressive.
- Build a simpler structural query on sutra's existing tree-sitter
  infrastructure. Snippet matching + metavariables + where clauses would cover
  80% of use cases with 20% of GritQL's complexity.
- Hybrid: use sutra's HRR similarity to *detect* conventions automatically,
  then express them as structural patterns for *checking*.

**Key GritQL techniques that apply:**

- Snippet-as-pattern: users write code to describe code. No AST knowledge
  needed.
- Equivalence classes: `'foo'` and `"foo"` match each other. Controlled
  fuzziness reduces false positives.
- Disregarded fields: per-language "don't-care" fields for matching.
  `async fn` and `fn` match the same pattern unless you specifically care
  about async.
- Text hoisting: raw substring scan before tree-sitter parsing. Only parse
  files that could possibly match. Makes convention checking fast enough to
  run on every commit.
- Pattern hash caching: content-addressed negative result caching. Files that
  passed last time with the same rules skip rechecking.

### Layer 2 — Constraint checking at write time

**Source**: GitNexus health metrics, sutra orient mode design.

Orient mode already surfaces constraints, dependencies, and conventions. The
shift: make these *prescriptive gates* rather than *informational context*.

**What this looks like in practice:**

An agent calls `sutra_orient` before modifying `src/db/connection.rs`. The
response includes:

```
constraints: [
  "No direct dependencies from db → http (architectural boundary)",
  "All public functions must be #[instrument] traced (convention, 34/35 comply)",
],
health: {
  instability: 0.82,  // high — many dependents, be careful
  cohesion: 0.71,     // acceptable
},
guard_rules: [
  "Adding a dependency on a module with instability > 0.9 requires justification",
]
```

After modification, `sutra_review` checks whether constraints were respected.
If the agent added `use crate::http::Client`, sutra flags the architectural
boundary violation — not as a style issue, but as a structural rule backed
by the dependency graph.

**Health metrics as gates (from GitNexus):**

- If a commit drops a component's cohesion below threshold → warning
- If a change increases a module's fan-out beyond its historical range → flag
- If a small change impacts many execution flows → "blast radius larger than
  expected" signal
- If a new dependency targets the most unstable component → require
  justification

**Execution flow impact (from GitNexus):**

- Map diffs to affected execution flows (processes)
- "This change to the parser affects the import flow, the export flow, and
  the validation flow" — quantitative blast radius
- If the stated intent was "fix import parsing" but 3 other flows are
  affected, that's a signal worth surfacing

### Layer 3 — Requirements/design validation

**Source**: Kiro's SMT-based requirements analysis.

The most upstream intervention: validate that the *intent* is coherent before
code is written.

**What Kiro does:**

Three-stage neurosymbolic pipeline:

1. **Refinement**: LLM rewrites vague requirements into testable acceptance
   criteria (EARS format: "When X, the system shall Y").
2. **Auto-formalization**: LLM translates criteria into SMT-LIB formal logic
   (schema declarations, assertions, background constraints).
3. **Logical analysis**: SMT solver proves whether the logic is satisfiable.
   Catches contradictions, ambiguities, incompleteness, wrong abstraction
   level.

Key findings: 60% of first-draft requirements contain bugs before any code
is written. LLM code generation accuracy drops 20-40% with ambiguous or
incomplete specifications.

**Semantic entropy as an ambiguity detector:**

The clever technique: intentionally sample multiple LLM formalizations of the
same requirement. If different samples produce different formal logic, the
requirement is ambiguous. Low entropy = confident, use it. High entropy =
requirement needs rewriting. Medium entropy = surface a clarification question.
This turns LLM non-determinism from a liability into a signal.

**How sutra could apply this:**

Full SMT-based requirements verification is a large undertaking. But elements
are adoptable:

- **Constraint consistency checking**: Given a proposed change description and
  the existing architectural rules (from the component model + dependency
  graph), check whether the proposal is consistent. "You say this module
  should be independent, but the change requires importing from the
  persistence layer."

- **Semantic entropy for any LLM-mediated step**: Apply the multi-sample
  divergence check to component labeling, convention descriptions, orient
  summaries, review findings. If multiple samples disagree, the underlying
  signal is ambiguous — surface it rather than committing to a
  confident-sounding guess.

- **Acceptance criteria extraction**: Before an agent implements a task, have
  it (or a separate agent) produce testable acceptance criteria. Run those
  through consistency checking. This doesn't require a full SMT solver — even
  LLM-based contradiction detection catches obvious issues.

- **Property extraction from specs**: Translate acceptance criteria into
  property-based test specifications. "When a user submits an order with
  inventory available, the order shall be fulfilled" becomes a property that
  can be tested with hundreds of generated inputs.

## Rust verification tools

Existing tools in the Rust ecosystem that fit the "validate agent-written
code without reading every line" model.

### Kani — bounded model checking

Amazon's model checker for Rust. You write `#[kani::proof]` harnesses and it
mathematically proves absence of panics, overflows, out-of-bounds, etc. via
CBMC (C Bounded Model Checker). The code-level analog of Kiro's spec-level
SMT. An agent writes a function; a Kani harness proves it can't crash for any
input up to a bound. You don't need to read the implementation — you read the
proof result.

Supports incremental verification: only re-verify code affected by changes.
For critical paths (parsers, state machines, protocol handlers), this provides
guarantees that no amount of testing can match.

### MIRI — undefined behavior detection

Rust's official UB detector. Runs code in an interpreter that catches memory
safety violations, data races, and undefined behavior the compiler can't see.
Especially valuable for unsafe code blocks. Zero-configuration:
`cargo +nightly miri test` just works. Should be a standard gate for any
agent-written code that uses `unsafe`.

### cargo-mutants — mutation testing

Systematically introduces bugs (change `>` to `>=`, replace return values
with `Default::default()`) and checks whether tests catch them. If a mutant
survives, the tests are inadequate. Answers the meta-question: "do the tests
actually test anything?" Agents are prone to writing tests that pass but
don't actually constrain behavior — mutation testing exposes this.

### proptest / quickcheck — property-based testing

Instead of "this input gives this output," express "for all inputs satisfying
X, the output satisfies Y." The framework generates hundreds or thousands of
random inputs. Catches edge cases that example-based tests miss. An agent can
write the property; the framework does the exploration. Connects directly to
Layer 3's property extraction from specs — acceptance criteria become
properties, properties become proptest harnesses.

### cargo-semver-checks — API compatibility

Detects accidental API breaking changes by comparing the current public API
surface against the previous version. If sutra tracks public API surfaces,
this catches when an agent unintentionally changes a public interface that
other code depends on.

### Stateright — TLA+-like model checking in Rust

For concurrent or distributed code. Model the system's states and transitions;
the checker exhaustively explores reachable states looking for invariant
violations, deadlocks, livelocks. This is how Amazon verified their
distributed systems (they used TLA+; Stateright brings the same approach into
Rust). Heavy, but for critical subsystems it provides guarantees about
concurrent correctness.

## Techniques that don't require reading the implementation

These matter specifically for the workflow where the human works at a high
level and agents write the Rust.

### Specification mining

Daikon-style automatic invariant inference. Observe existing code and tests
to infer likely invariants: "this function always returns a positive number,"
"this field is never None after init," "the output length is always ≤ the
input length." Then check new code against inferred specs. The human reviews
*invariants* (high-level, readable), not *implementation* (low-level Rust).

Sutra could mine invariants from existing code via test execution traces or
static analysis, then surface violations when agent-written code breaks them.

### Behavioral contracts

Preconditions, postconditions, invariants expressed declaratively. Rust
crates like `contracts` add `#[pre]` / `#[post]` / `#[invariant]` attributes.
The agent writes the implementation; the human reviews the contract. Contracts
are high-level enough to read without deep Rust knowledge, and they're
machine-checkable at runtime.

Example: `#[post(ret.len() <= input.len())]` — you can read that and judge
whether it's the right constraint without understanding the implementation.

### Topology-aware review prioritization

Not all code changes are equally risky. Changes to high-fan-in code (many
dependents) need more scrutiny than changes to leaf modules. Sutra already
has the graph to compute this. Surfacing "this change is in a critical path,
review carefully" vs "this is an isolated utility, lower risk" lets the human
allocate attention where it matters most.

Could be expressed as a simple risk score: `risk = fan_in * change_magnitude
* instability`. High-risk changes get human review; low-risk changes can
pass with automated gates only.

### Semantic diff

Not "what lines changed" but "what behavior changed." Two functions can have
different text but identical behavior (refactoring), or near-identical text
but different behavior (subtle bug). Sutra's HRR vectors could power this:
if the structural signature of a function changed significantly, flag it even
if the text diff looks small. Conversely, a large text diff with stable HRR
signature = probably safe refactoring.

### Test adequacy metrics

Beyond "tests pass," measure whether tests are *sufficient*. Mutation testing
survival rate is the strongest signal. Branch coverage on specifically the
changed code is more actionable than whole-project coverage. If an agent
claims "I added tests," test adequacy metrics answer "yes, but are they
good tests?"

## Agent workflow approaches

Patterns for the specific dynamic where agents write code and humans
validate.

### Adversarial pairing

Have one agent write code, a second agent try to break it — write failing
tests, find edge cases, challenge assumptions, look for off-by-one errors
and unhandled states. The first agent fixes. The human reviews the
*dialogue* and the *final result*, not every intermediate step. Red team /
blue team for code.

This naturally produces better test coverage because the adversary is
motivated to find gaps, not confirm correctness.

### Graduated trust with automated gates

New or modified code starts at low trust. It must pass progressively harder
checks to "graduate":

```
type-check → lint → unit tests → property tests →
  mutation testing → integration tests → model checking (critical paths)
```

The human only reviews code that passed all automated gates. The gates are
the quality filter, not the human's line-by-line reading. Sutra's role:
orchestrate the gate sequence and surface results, with the graph determining
which gates apply (leaf utility = lighter gates; core state machine = full
gates including Kani).

### Intent-to-verification pipeline

The human states intent ("add rate limiting to the API"). A first agent
extracts testable properties:

- "No client can exceed N requests per window"
- "Rate-limited requests get 429"
- "The window resets after T seconds"
- "Rate limit state survives server restart"

A second agent writes the implementation. The properties become automated
tests (property-based or conventional). The human reviews the *properties*
(high-level, readable) not the *implementation* (low-level Rust). If the
properties are right and the tests pass, the implementation is constrained
to be correct.

### Architectural decision records as constraints

When the human makes a high-level decision ("we use event sourcing, not
CRUD"), it gets encoded as a sutra constraint — not just a doc, but a
checkable rule in the dependency graph or convention set. Agents are informed
of it via orient mode. If they violate it, sutra catches it structurally, not
by hoping the agent remembers an instruction from three sessions ago.

These accumulate as the project evolves: each decision narrows the space of
valid implementations. Over time, the constraint set becomes a machine-
readable architecture specification.

## Type-level quality patterns

Rust's type system is powerful enough to encode many invariants at compile
time. Agents that use these patterns get verification for free from the
compiler. Sutra could detect when code *should* use these patterns but
doesn't.

### Session types / typestate

Encode protocols as types so the compiler rejects invalid sequences.
"A connection must follow connect → authenticate → query → disconnect"
becomes a type-level state machine where calling `.query()` on an
unauthenticated connection is a compile error, not a runtime error.

Sutra could detect protocol-like patterns (methods that must be called in
order, resources with lifecycle phases) and suggest typestate encoding. Or
flag when an agent bypasses an existing typestate pattern with unsafe casts
or raw pointers.

### Newtype / branded types

Wrapping primitive types to distinguish them at the type level. `UserId(u64)`
vs `OrderId(u64)` — same underlying type, but the compiler prevents mixing
them up. Agents that use raw `u64` where a newtype exists are producing slop
that the compiler could have caught.

Sutra could detect "this function takes three u64 arguments with different
semantic meanings" and flag it as a newtype opportunity.

## Continuous and temporal analysis

### Continuous background verification

Don't wait for PR time. Run verification tools continuously as a daemon (sutra
already runs as a daemon for indexing). Changes trigger re-verification of
affected code. By the time a human looks at it, the verification results are
already attached to the change. The human sees "all gates passed" or "Kani
found a panic on line 47" without having to run anything.

### Convention drift detection over time

Not just "does this code match conventions now" but "are conventions drifting
across commits?" If each agent session introduces slightly different patterns,
the codebase is diverging even if each individual change looks fine. Track
convention cohesion as a time series. Alert when the variance of structural
patterns within a component exceeds a threshold.

This catches a failure mode specific to multi-agent workflows: no single
agent produces bad code, but they collectively produce an inconsistent
codebase because each has slightly different habits.

### Explain-your-choices as a quality signal

Require agents to annotate non-obvious decisions: why this data structure?
Why this error handling strategy? Why this concurrency model? Not comments
in code — metadata attached to the commit or task. If an agent can't explain
a choice coherently, that's a smell.

Sutra could compare the explanation against the actual code structure: "the
agent said it chose a HashMap for O(1) lookup, but the map is iterated
linearly in 4 out of 5 call sites" — the explanation doesn't match the usage
pattern.

## Feedback loops

The three layers work together:

The three layers work together:

```
Intent → [Layer 3: validate spec] →
  Design → [Layer 2: check constraints] →
    Code → [Layer 1: enforce conventions] →
      Review → [sutra_review: verify everything]
```

Each layer catches a different class of slop:

| Layer | Catches |
|-------|---------|
| 3 — Spec validation | Contradictions, ambiguity, missing cases |
| 2 — Constraint checking | Architectural violations, unexpected coupling |
| 1 — Convention enforcement | Style drift, pattern violations, structural inconsistency |
| Review | Everything above + semantic correctness, performance, security |

The earlier a problem is caught, the less rework is needed. Layer 3 prevents
building the wrong thing. Layer 2 prevents building it in the wrong place.
Layer 1 prevents building it in the wrong way.

## Sources

- GritQL: structural query/rewrite language (Rust, tree-sitter based).
  Surveyed in `survey-gitnexus-gritql-seeflow.md`.
- GitNexus: code intelligence knowledge graph. Health metrics, execution
  flows, detect_changes. Surveyed in same doc.
- Kiro Requirements Analysis: neurosymbolic pipeline using SMT solvers to
  catch requirement bugs before implementation. AWS/Amazon, May 2026.
  https://kiro.dev/blog/deep-spec-analysis/
