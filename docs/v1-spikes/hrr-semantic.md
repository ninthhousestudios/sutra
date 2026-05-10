# spike: HRR semantic & temporal binding

## status: continuing (structural spike complete)

## question

Can HRR serve as the substrate for Sutra's full associative knowledge
layer — not just structural code features, but experiential knowledge,
temporal evolution, and agent-contributed annotations?

## context

The structural encoding spike proved HRR works for representing function
AST shape and doing algebraic operations (search, decomposition,
transforms, cross-file diff). But Sutra v1 needs more than structure.
A long-tenured engineer's intuition includes "this is paywall code,"
"this area is fragile," "the last three changes here introduced bugs."
That knowledge must live in the same vector space as structural features
so similarity retrieval surfaces both.

## experiments

### 1. semantic tag binding

Bind agent-contributed tags into function/module vectors. Tags like
`concern:paywall`, `risk:high`, `pattern:state-machine`.

- Encode functions with structural features (as now)
- Bind semantic tags: `fn_vec + bind(tag_role, tag_value)`
- Test: does similarity search still work? If I query "functions similar
  to this paywall function," do structurally similar AND semantically
  tagged functions both surface?
- Test: can I unbind to recover tags? Given a function vector, extract
  what semantic tags are bound into it.
- Measure: at what point do semantic bindings degrade structural
  similarity? (interference threshold)

### 2. temporal binding

Encode the same function at two points in time (two commits). Compute
a vector diff.

- Parse function at commit A, encode. Parse at commit B, encode.
- Diff = vec_B - vec_A (HRR supports subtraction)
- Test: is the diff interpretable? Does it recover the structural
  change (e.g., "added error handling," "removed unsafe block")?
- Test: can the diff predict related changes? "This function gained
  a loop — what other functions also gained loops in this commit range?"
- Compare against: git diff (textual), sutra_diff_impact (structural)

### 3. codebook scaling

The current codebook has 89-116 entries (structural traits). Semantic
and temporal features add dimensions.

- Incrementally grow the codebook with semantic categories
- Measure: retrieval quality vs codebook size curve
- Find: saturation point (where adding entries stops helping)
- Find: interference floor (where the vector space gets too crowded)

### 4. composition with structured queries

The payoff: can HRR rank results from the Datalog/structured layer?

- Datalog gives blast radius: "14 callers affected by this change"
- HRR ranks them: "but these 3 are semantically closest to paywall code"
- Prototype: take sutra_impact output, re-rank by HRR similarity
- Measure: does re-ranking match human judgment about what's relevant?

## inputs

- Existing hdc-eval infrastructure and codebook
- Redox kernel and linux kernel test corpora
- A smaller project with known "subsystem" boundaries (for semantic
  tag ground truth)

## done criteria

- Semantic binding: P@5 for mixed structural+semantic queries within
  20% of pure structural baseline
- Temporal: >40% of diffs recover the correct structural change category
- Codebook: identified saturation point, documented interference threshold
- Composition: prototype of HRR-ranked impact results exists and is
  qualitatively evaluated

## verdict format

Written verdict: viable / viable with caveats / not viable.
Include: what works, what doesn't, performance envelope,
architectural constraints for integration.

## timebox

2-3 weeks.
