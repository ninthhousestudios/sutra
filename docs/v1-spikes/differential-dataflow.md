# spike: differential dataflow

## status: not started

## question

Can differential dataflow replace Sutra's current ad-hoc graph
analyses with a unified, automatically incremental, temporally aware
reasoning engine?

## context

Sutra currently computes dependencies, impact, co-change, and other
analyses procedurally — each tool reimplements its own graph traversal
and caching logic. Differential dataflow (Frank McSherry, Rust-native)
provides declarative Datalog-like queries with automatic incrementality:
insert/retract facts, and all derived views update with only the delta.
It also handles time natively, enabling "what was true at time T?"
queries.

If this works, it becomes Sutra's structured reasoning backbone —
facts, rules, constraints, incremental updates, and temporal queries
in one framework.

## experiments

### 1. fact encoding

Define the base relations for a codebase:

- `file(id, path, language)`
- `symbol(id, name, kind, file_id, span)`
- `calls(caller_id, callee_id)`
- `imports(file_id, target_file_id)`
- `contains(parent_id, child_id)`

Populate from Sutra's existing tree-sitter parse output.
Measure: ingestion time, memory footprint, comparison to current storage.

### 2. port existing analyses

Rewrite three core Sutra analyses as differential dataflow computations:

**deps** (transitive dependencies):
```
deps(a, b) :- imports(a, b).
deps(a, c) :- imports(a, b), deps(b, c).
```

**impact** (what's affected by changing symbol X):
```
affected(x) :- target(x).
affected(y) :- affected(x), calls(y, x).
affected(y) :- affected(x), contains(x, y).
```

**co-change** (files that change together):
```
cochange(a, b, n) :- commit_touches(c, a), commit_touches(c, b),
                     a != b, n = count(c).
```

Measure: correctness (same results as current implementation?),
performance (faster/slower?), code complexity (LOC comparison).

### 3. incremental update

Simulate a file change:
- Retract old facts for modified file
- Insert new facts from re-parse
- Measure time for all derived views to converge
- Compare to current approach (full recomputation? partial?)

Target: incremental update under 50ms for a single-file change
in a 10K-function codebase.

### 4. architectural constraints as maintained views

Express rules that should always hold:

- "No circular dependencies between modules"
- "Public API functions have doc comments" 
- "Test files don't import from other test files"

These become views that the system continuously maintains. When a
change violates one, it's immediately visible — not discovered by
running a lint pass later.

### 5. temporal queries

Using differential dataflow's time dimension:

- "When did this dependency first appear?"
- "What was the call graph of module X at commit Y?"
- "How has the dependency count of this file changed over the last
  20 commits?"

Evaluate whether the temporal model maps cleanly to git history
or requires an adapter layer.

## inputs

- Sutra's current parse output (symbol table, call graph, imports)
- differential-dataflow and timely-dataflow Rust crates
- Test codebases: sutra itself (~70 files), redox kernel (~200 files),
  a larger project for scale testing

## done criteria

- 3 core analyses ported and producing identical results to current impl
- Incremental update < 100ms for single-file change on 1K-function codebase
- At least 2 architectural constraints expressed and continuously maintained
- Temporal query demonstrated (any one of the above)
- LOC comparison and ergonomics assessment written up

## risks

- Differential dataflow has a learning curve; the timely dataflow
  execution model is non-trivial
- May be overkill if Sutra's analyses stay simple enough for ad-hoc
  graph traversal
- Memory overhead of maintaining all derived views simultaneously

## verdict format

Written verdict: viable / viable with caveats / not viable.
Include: programming model assessment, performance measurements,
what it simplifies vs what it complicates, integration path with
existing Sutra code.

## timebox

2-3 weeks.
