# sutra vision review

This note is a critique of `docs/sutra-vision.md` from the visioning stage.
It is meant to be folded back into the main vision, not treated as a competing
document.

## Overall read

The vision is strong. The core idea is coherent and valuable: sutra is not
better code search or another linter. It is a persistent architectural
substrate for human-agent software work.

The strongest part of the vision is the separation between discovered
structure, human intent, agent orientation, and verification. Most AI coding
tools collapse those into prompt context or review comments. Sutra's bet is
that architecture needs to become a living, queryable, enforceable model. That
bet feels right.

The main risk is scope. The vision currently combines three products that may
each be hard enough on their own:

1. Architectural model and component understanding.
2. Constraint and convention enforcement.
3. Behavioral correctness verification.

They belong in the same long-term vision, but behavioral verification should
not distort the identity too early. Verification promises confidence without
line-by-line reading, but it may be much harder to make broadly useful than
architectural fit.

The sharpest version of sutra is:

> Sutra tells an agent what kind of code belongs here, then tells the human
> whether the resulting change preserved the architecture.

That is already a large and differentiated product.

## Core user loop

The current vision has a strong layer model, but the day-to-day loop is
implicit. The vision would benefit from a concrete core loop:

1. Human or agent selects a task.
2. Sutra orients the agent to the relevant architecture.
3. Agent writes code.
4. Sutra checks the diff for architectural fit and convention drift.
5. Sutra produces a review report the human can trust.
6. Human accepts, rejects, or teaches sutra by updating constraints, aliases,
   and conventions.

This loop may clarify the MVP better than the layer stack alone.

## MVP bias

For an initial product slice, bias toward:

- Layer 0: solid structural facts.
- Layer 1: components, even if initially semi-manual.
- Layer 2: conventions, advisory only.
- Layer 3: explicit constraints.
- A strong `sutra orient` and `sutra review`.

Defer deep Layer 7 behavioral verification until the architectural
orient/review loop is excellent.

## Blind spots and risks

### Architecture is partly social

Graph clustering can discover cohesion, but it may miss intended boundaries
that are organizational, historical, conceptual, or strategic. Sometimes two
modules talk constantly but should remain separate. Sometimes two files never
interact but belong to the same feature concept.

The vision acknowledges human input, but this should be elevated: discovered
architecture is a proposal, not truth. The living model needs provenance for
each claim:

- computed
- inferred
- human-authored
- ADR-derived
- agent-learned

### False positives are existential

If sutra becomes noisy, humans and agents will route around it. The vision
should say more about confidence, severity, suppressions, waivers, and
advisory-vs-blocking modes.

A boundary violation, a weak convention deviation, and a health regression
should not all feel like the same kind of failure. Sutra needs a trust model,
not just a rule model.

### Convention detection can encode bad habits

"Patterns the code actually follows" is powerful, but legacy code often
follows the wrong pattern. FCA may detect conventions that are merely
accidents, stale design, or local compromises.

The convention model should distinguish:

- descriptive convention: this is common
- preferred convention: this should continue
- deprecated convention: this exists but should fade
- forbidden convention: do not copy this

That distinction may be central to making conventions useful for agents.

### Review UX may be the heart of the product

The vision says "verify without line-by-line reading," but the real product
question is what artifact the architect actually looks at.

The first great sutra experience may be `sutra review`, producing an
architectural change report:

- components touched
- new dependencies
- boundary crossings
- conventions followed or broken
- semantic anchors changed
- health deltas
- verification evidence
- recommended files to inspect manually

This report may be more important than a dashboard.

### Intent needs a clearer place in the model

The doc distinguishes constraints, conventions, aliases, ADRs, and
verification properties, but design intent is scattered across them.

Consider making intent a first-class concept: decisions, invariants,
boundaries, allowed dependencies, expected behaviors, ownership, and lifecycle
state. Otherwise "intent" remains the thing sutra claims to preserve but does
not quite model directly.

### Multi-language support needs capability levels

A language-agnostic core is the right strategic choice, but the product will
only feel intelligent where the language adapter is deep. Rust can support
richer facts than Python or Dart.

The core should avoid pretending all languages produce equal architectural
signal. Each adapter should declare which facts it can produce and with what
confidence.

### Agent integration needs canonical interactions

"Orient" is excellent, but it should become more concrete. Define a small
number of canonical agent interactions:

- "I am about to edit X; orient me."
- "Here is my plan; check architectural fit."
- "Here is my diff; review architectural delta."
- "Give me local templates and conventions for adding Y."

This would make the collaborator identity much more concrete.

### Stability may matter more than perfect accuracy

The vision mentions stable components, which is important. Push this harder.
If sutra renames or reclusters components frequently, humans will not trust the
model.

The architecture model probably needs identity, history, merges, splits,
aliases, and drift tracking. "This component used to be X and is becoming Y"
is more useful than a freshly computed cluster.

### Verification must include what was not verified

For human trust, sutra must be explicit about gaps. Not just "property tests
passed," but:

- no contract exists for this behavior
- mutation score is weak
- this change touched high-risk logic with no behavioral evidence
- this property was inferred but not human-approved

Negative evidence and missing evidence should be first-class.

### Product boundaries will overlap

The doc defines boundaries with yojana, vidhi, and chitta well, but in
practice they will overlap around intent.

If yojana has task intent and vidhi has design rationale, sutra may need
references into those systems rather than ownership of that data. A useful
boundary may be:

> Sutra owns codebase facts and enforceable architectural state. Other systems
> own why the work exists.

## Suggested additions to the vision

Add a section for the core user loop, because it grounds the layered model in
the daily experience.

Add a trust model that covers confidence, severity, waivers, suppressions,
advisory checks, and blocking checks.

Add provenance to the living model. Every architectural claim should know
whether it was computed, inferred, human-authored, ADR-derived, or
agent-learned.

Add a convention lifecycle. Detected conventions should be promoted,
deprecated, forbidden, or left descriptive.

Add a canonical review artifact. The clearest product surface may be the
architectural change report produced by `sutra review`.

Add capability levels for language adapters so the core can be
language-agnostic without flattening away real language differences.

## Bottom line

The vision is real and differentiated. The blind spot is not ambition; the
ambition is appropriate. The risk is trying to prove too many forms of
intelligence at once.

If sutra first becomes the best architectural orient/review system for agents,
the rest has a strong foundation to grow from.
