# spike: formal concept analysis

## status: not started

## question

Can FCA automatically detect meaningful codebase conventions and
produce a concept hierarchy that agents can use to check their work
against the project's own patterns?

## context

A long-tenured engineer knows "every error type in this codebase
implements Display" and "handler functions always validate auth first"
— not because someone wrote it down, but because they've seen the
pattern enough times. FCA formalizes this: given a set of code elements
(objects) and their properties (attributes), it extracts the natural
concept lattice — the hierarchy of "things that share properties."

Implications in the lattice are detected conventions:
"everything with attribute A also has attribute B." Violations are
code elements that break an implication. This is the Panini-inspired
convention layer, made computational.

## experiments

### 1. context construction

Build a formal context (objects x attributes matrix) from a real
codebase:

Objects: functions, types, modules, traits/impls

Attributes (candidates — prune based on what's informative):
- Structural: returns_result, has_unsafe, uses_loop, has_match,
  is_async, is_pub, has_doc_comment, has_test, takes_self
- Relational: calls_external_crate, has_callers, is_leaf,
  is_in_module_X
- Historical: recently_changed, high_churn, multiple_authors
- Derived: high_complexity, large_function, many_parameters

Feed from Sutra's existing parse + git analysis output.

### 2. lattice extraction

Run FCA on 3 codebases of different sizes and styles:
- Small/focused: sutra itself (~70 files, Rust)
- Medium: redox kernel (~200 files, Rust)
- Large/diverse: a multi-crate workspace

Measure:
- Number of concepts in the lattice
- Number of implications (candidate conventions)
- Lattice depth and width

### 3. convention quality assessment

For each extracted implication, manually evaluate:

- **True convention:** "all pub functions in this module return Result"
  — yes, that's a real pattern the developers follow.
- **Coincidence:** "all functions with 3 parameters use unsafe" — true
  in the data but not a meaningful convention.
- **Meaningful exception:** "all error types implement Display except
  InternalError" — the exception is deliberate.

Target: >50% of implications with support >= 5 are true conventions
(not coincidences).

### 4. violation detection

Introduce deliberate violations into a codebase:
- Add a pub function that doesn't return Result (in a module where
  all others do)
- Add an error type that doesn't implement Display
- Add a handler that skips auth validation

Measure: does FCA flag these as violations of detected conventions?
What's the false positive rate on unchanged code?

### 5. incremental update

When a file changes, can the concept lattice update incrementally?
Or does it need full recomputation?

- Measure: recomputation time for full lattice on each codebase
- If too slow for incremental: can you maintain a "recent violations"
  view that only checks new/changed code against existing implications?

## inputs

- Sutra's parse output (symbol kinds, properties, containment)
- Sutra's git analysis output (churn, authors, co-change)
- An FCA library (concepts crate in Rust, or call out to a Python
  implementation for the spike)
- Test codebases with known conventions (for ground truth)

## done criteria

- Concept lattice extracted from at least 2 real codebases
- >50% precision on extracted conventions (support >= 5)
- Deliberate violations detected with >80% recall
- False positive rate on unchanged code < 20%
- Incremental update path identified (even if not fully implemented)
- Assessment of concept lattice navigability (can an agent usefully
  browse it?)

## risks

- Lattice explosion: too many attributes → combinatorial blowup in
  concepts. May need attribute selection or pruning.
- Coincidental implications: statistical noise masquerading as
  conventions. Minimum support thresholds help but may not be enough.
- May require human curation to be useful (defeats the "automatic"
  goal).

## verdict format

Written verdict: viable / viable with caveats / not viable.
Include: sample conventions extracted (the good and the bad),
precision/recall numbers, scale assessment, integration path
(how would this feed into Sutra's agent-facing tools?).

## timebox

2 weeks.
