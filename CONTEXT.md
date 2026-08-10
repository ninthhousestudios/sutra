# Sutra

A living architectural model of the codebase — discovered from code, refined
by human intent, enforced during development — that gives both humans and
agents the shared understanding needed to build coherent software together.

## Language

### Core model

**Component**:
A named group of symbols. Discovered by clustering, stabilized by human
naming. Not tied to filesystem structure — a directory may contain multiple
components, a component may span directories. The filesystem is a hint, not
a definition.
_Avoid_: module (too tied to language-level constructs), package, folder

**Symbol**:
Any named declaration extractable by tree-sitter that participates in the
dependency graph. The exact set varies by language adapter. The criterion:
if it has a name and other code can reference it, it's a symbol.
_Avoid_: entity, node, element

**Edge**:
A typed relationship between two symbols. Carries a kind that determines
which analyses use it. Not all edges are equivalent — a boundary check uses
import edges, blast radius propagates through call edges, clustering uses
calls + imports but not containment.
_Avoid_: link, connection, reference (too vague)

**Boundary**:
The set of allowed dependencies between two components. Directional — A may
depend on B but not vice versa. Always declared (Layer 3), never discovered.
Observing that two components don't depend on each other is a fact, not a
boundary. A boundary is the human saying "keep it that way."
_Avoid_: wall, barrier, separation (too absolute — boundaries define what
crosses, not that nothing crosses)

**Semantic anchor**:
A symbol that is architecturally central to its component — removing or
changing it would reshape the component's role. Identified heuristically
(high fan-in, stable across history, prominent naming), confirmable by the
human. Discovered anchors are proposals; human-confirmed anchors are
authoritative. What Review flags when mutated.
_Avoid_: key symbol, core type (too informal)

### Rules and patterns

**Convention**:
A pattern discovered from code — what the code actually does. Detected by
FCA from the symbol-attribute matrix. Conventions are descriptive — they
describe reality, not intent, and are advisory: what the code does and should
continue doing, not an absolute rule.
_Avoid_: rule (implies prescription), standard, norm

**Constraint**:
A rule declared by a human — what the code must do. Prescriptive and binding
regardless of what the code currently does. Written directly for sutra or
extracted from ADRs. A convention cannot become a constraint;
if you want absolute enforcement, write a constraint.
_Avoid_: policy (too organizational), convention (different concept)

### Provenance

**Provenance**:
The origin of any claim in the architectural model. Every fact, convention,
boundary, and anchor carries provenance so the architect can audit what sutra
believes and why. Discovered architecture is a proposal, not truth.
Human-authored claims override inferred ones.

**Computed provenance**:
Deterministic extraction from code. Two independent implementations would
necessarily agree. Symbol existence, call edges, import relationships,
fan-in counts.

**Inferred provenance**:
Statistical or heuristic derivation. The answer depends on thresholds,
algorithm choice, or pattern detection. Cluster membership, convention
detection, anchor identification. Two implementations might reasonably
disagree.

**Human-authored provenance**:
Written directly for sutra by a human. Constraints, component boundaries,
aliases.

**ADR-derived provenance**:
Extracted from an architectural decision record written for humans. The
extraction step matters — sutra's interpretation could be wrong. Provenance
tells you "if this seems off, check the ADR."

**Agent-learned provenance**:
Auto-captured from agent sessions. Applies specifically to Layer 5
vocabulary — concept-to-code mappings observed from agent behavior. Agents
do not get to reshape architecture, conventions, or constraints by
repetition.

### Trust model

**Severity**:
How a finding should be treated. Not all findings are the same kind of
failure.
- **blocking** — must be addressed before merge (constraint failures,
  boundary violations)
- **advisory** — flagged for human judgment (health regressions)
- **informational** — reported but not flagged (new dependencies within
  allowed boundaries)

**Waiver**:
A human decision to accept a specific finding, with recorded rationale.
Targets a specific finding, not a category or file. Requires rationale,
appears in every review report that touches the waived area, can be revoked.
An assertion of intent — "this looks wrong but is what I want here" — not a
silencing.
_Avoid_: suppression (implies hiding), ignore, disable

### Core loop

**Explore**:
Agent asks sutra to find the relevant code before writing. Conventions,
lessons, semantic anchors, and cautions for the area surface contextually as
it reads (sutra_read, sutra_impact) — understanding arrives with the code
rather than through a separate briefing step.

**Check**:
Incremental, real-time architectural validation. Runs on each file change
during a session via the propagation path (Layer 0 delta through DD, HRR,
FCA). Answers "did this edit just break something?" Fast, narrow, immediate.
The guardian during writing.

**Review**:
Holistic architectural summary of a complete change. Runs on a branch, PR,
or set of commits and produces the architectural change report. Answers
"what did this whole change do to the architecture?" Slower, broader,
deliberate. The report after writing.

**Teach**:
Human refines sutra's model. Updates constraints, component boundaries,
aliases, or confirms/rejects inferred claims.
How sutra learns from corrections.

### Artifacts

**Architectural change report**:
A structural diff describing how the architecture changed — not just what
rules were violated. Three sections:
1. **What changed structurally** — new/removed edges, components touched,
   anchors mutated, component growth. Factual. Present even when nothing is
   wrong.
2. **What rules were violated** — boundary crossings and constraint failures.
   Findings with severity and provenance.
3. **What wasn't verified** — verification gaps. No contract, weak mutation
   score, high-risk logic with no behavioral evidence.

The differentiator from a linter: section 1 exists even when sections 2 and
3 are empty. A change that breaks no rules can still be architecturally
significant.

## Edge kinds

Base kinds sutra defines (adapters may add language-specific kinds):

- **contains** — structural nesting (module contains symbol)
- **calls** — runtime invocation
- **imports** — compile-time dependency
- **implements** — type relationship (trait impl, interface impl)

Analyses declare which edge kinds they operate on. Boundary constraints check
imports and calls. Blast radius propagates through calls. Clustering uses
calls + imports but not containment.

## Component identity

- Components have identity and history — clustering changes are tracked as
  merges, splits, or drifts, not fresh sets.

## Relationships

- A **Component** contains one or more **Symbols**
- **Symbols** are connected by **Edges** of specific kinds
- **Boundaries** govern the allowed **Edges** between **Components**
- **Conventions** are detected from **Symbol** attributes within a **Component**
- **Constraints** are declared by humans and checked against **Edges** and **Symbols**
- **Semantic anchors** are the architecturally central **Symbols** of a **Component**
- **Explore** surfaces **Conventions**, **Lessons**, and **Anchors** to agents as they read
- **Check** validates **Edges** and **Symbols** against **Constraints** and **Conventions** in real time
- **Review** produces the **Architectural change report** from the delta of a complete change
- **Teach** updates **Constraints**, **Boundaries**, and **Component** definitions
- Every claim carries **Provenance**; every finding carries **Severity**
- **Waivers** accept specific findings from **Check** or **Review**

## Example dialogue

> **Dev:** "The agent added a call from auth to billing — is that a problem?"
> **Sutra (Review):** "There's no **boundary** between auth and billing, so
> it's not a violation. But the **architectural change report** shows a new
> **calls** edge between these **components**. Auth's **semantic anchor**
> `SessionManager` was not changed. No **conventions** were broken. Section 1
> shows the new dependency; sections 2 and 3 are clean."
> **Dev:** "Auth should never depend on billing. Add that as a constraint."
> **Sutra (Teach):** "Added **constraint**: auth must not import or call
> billing. **Provenance**: human-authored. The existing call is now a
> **blocking** finding."

## Flagged ambiguities

- "rule" was used interchangeably for conventions and constraints — resolved:
  conventions are discovered patterns (descriptive), constraints are declared
  rules (prescriptive). They differ by origin and authority.
- "suppress" was considered for accepting findings — resolved: **waiver** is
  the term. Suppression implies hiding; a waiver is a deliberate, tracked
  acceptance with rationale.
- "boundary" could mean component membership or inter-component dependency
  rules — resolved: boundary is strictly about inter-component dependency
  rules. Component membership is just "the component."
