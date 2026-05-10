# spike: salsa incremental computation

## status: not started

## question

Does Salsa fill a distinct role in Sutra's architecture (fine-grained
query memoization), or is it redundant with differential dataflow?

## context

Salsa is the incremental computation framework behind rust-analyzer.
It memoizes query results, tracks dependencies between computations,
and minimally re-executes when inputs change. It's proven at scale for
code intelligence workloads.

Differential dataflow and Salsa both solve "recompute efficiently when
inputs change," but at different granularities:
- Differential dataflow: relational/set-at-a-time, good for graph
  queries over the whole codebase
- Salsa: per-query memoization, good for "compute the outline of
  file X" where the result depends on that file's content only

They may be complementary (Salsa for per-file computations, differential
dataflow for cross-file reasoning) or one may subsume the other for
Sutra's workload. This spike clarifies the boundary.

## experiments

### 1. model Sutra's query hierarchy

Map Sutra's current tools to a Salsa query graph:

```
parse(file_path) -> AST           // depends on: file content
symbols(file_path) -> Vec<Symbol> // depends on: parse(file_path)
outline(file_path) -> Outline     // depends on: symbols(file_path)
calls(file_path) -> Vec<Call>     // depends on: parse(file_path)
deps(file_path) -> Vec<Dep>       // depends on: calls(file_path), 
                                  //             symbols(*)
```

Evaluate: does this dependency graph map cleanly to Salsa's model?
Where does per-file vs cross-file create friction?

### 2. prototype parse + outline

Implement the simplest Salsa pipeline: file content → parse → symbols
→ outline. Change a file, observe that only affected queries re-execute.

Measure:
- Cold compute time (full parse of workspace)
- Incremental time (change one file, recompute outline)
- Memory overhead of memoization tables

### 3. cross-file query

Implement a cross-file query in Salsa: "all callers of function X."
This depends on the call graph of every file.

Evaluate: does Salsa handle this gracefully, or does a change to any
file invalidate the entire query? If the latter, this is where
differential dataflow is better.

### 4. compare with differential dataflow

If the differential dataflow spike runs concurrently, compare:
- Same query, both frameworks: which is faster for cold? For
  incremental?
- Programming model: which is more natural for Sutra's workload?
- Can they coexist? (Salsa for per-file, differential for cross-file)

## inputs

- salsa crate (current version, not the 2022 rewrite — check status)
- Sutra's current parse + query code
- Test workspace: sutra itself

## done criteria

- Salsa query graph modeled for Sutra's core tools
- Parse → outline pipeline working with incremental updates
- Cross-file query attempted and assessed
- Clear written comparison with differential dataflow
- Recommendation: use Salsa, use differential dataflow, use both
  (with boundary defined), or use neither

## risks

- Salsa has had multiple major rewrites; API stability is a concern
- May not handle the relational/graph queries that are Sutra's core
  well
- If differential dataflow covers both per-file and cross-file,
  Salsa adds complexity without benefit

## verdict format

Written verdict: adopt / complement (with boundary) / skip.
Include: comparison matrix against differential dataflow,
programming model assessment, where each excels.

## timebox

1-2 weeks (lighter than other spikes — this is a boundary-finding
exercise, not a feature prototype).
