# Task 8: DD engine — cycle detection + lazy population + eviction

sutra/v1/8

## Context

Add a differential dataflow engine to sutra for incremental graph computation. First view: cycle detection. The dd-spike on branch `spike/hdc-ast-encoding` in `src/bin/dd-spike.rs` has reference patterns but should be reimplemented cleanly.

## Architecture

### Module structure: `src/dd/`

```
src/dd/
  mod.rs      — public types (Cycle, DdFacts, DdDelta), re-exports DdEngine
  engine.rs   — DdEngine struct: lazy init, eviction, public API
  worker.rs   — timely worker thread, command/result channels
  cycles.rs   — cycle detection dataflow definition
```

### Public API (no DD/timely types exposed)

```rust
// src/dd/mod.rs
pub struct Cycle {
    pub file_ids: Vec<i64>,
    pub paths: Vec<String>,
}

pub struct DdFacts {
    pub import_edges: Vec<(i64, i64)>,  // (file_id, resolved_file_id)
    pub ref_edges: Vec<(i64, i64)>,     // (file_id, target_file_id)
}

pub struct DdDelta {
    pub added_edges: Vec<(i64, i64)>,
    pub removed_edges: Vec<(i64, i64)>,
}
```

### DdEngine

```rust
// src/dd/engine.rs
pub struct DdEngine {
    state: Mutex<DdState>,
    idle_timeout: Duration,
}

enum DdState {
    Cold,
    Warm { handle: WorkerHandle, last_query: Instant },
}
```

**Methods:**
- `new(idle_timeout: Duration)` — creates Cold
- `ingest(facts: DdFacts) -> Result<()>` — Cold → Warm. Spawns timely worker thread, feeds edges, advances, waits for convergence
- `update(delta: DdDelta) -> Result<()>` — Warm only. Sends retractions/insertions at new timestamp
- `query_cycles() -> Result<Vec<Cycle>>` — Warm only. Returns current cycle set, updates last_query
- `evict_if_idle() -> bool` — if last_query exceeds idle_timeout, shutdown worker, go Cold
- `is_warm() -> bool`
- `Drop` impl: shutdown worker, join thread

### Worker thread (worker.rs)

Runs `timely::execute_directly`. Uses crossbeam channels for bidirectional communication.

Commands: `Ingest(Vec<(i64,i64)>)`, `Update(added, removed)`, `QueryCycles`, `Shutdown`

Results: `Cycles(Vec<Vec<i64>>)` (sets of file_ids per SCC)

Pattern:
1. Create `InputSession<usize, (i64, i64), isize>` for edges
2. Build cycle detection dataflow
3. Enter command loop, step to convergence after each mutation
4. Read output arrangement on QueryCycles, send back via result channel

### Cycle detection dataflow (cycles.rs)

Transitive closure via iterate:
```rust
let reachable = edges.iterate(|inner| {
    let edges_inner = edges.enter(&inner.scope());
    inner
        .join_map(&edges_inner, |_mid, &src, &dst| (src, dst))
        .concat(&edges_inner)
        .distinct()
});
```

Cycle nodes = `reachable.filter(|(src, dst)| src == dst).map(|(s,_)| s).distinct()`

Group into SCCs by finding connected components among cycle-participating nodes.

### Lazy population

DdEngine starts Cold. Integration point (future tool or SutraServer):
1. Load facts: `db.import_edges()` + `db.all_resolved_refs()` + `db.all_symbol_file_map()` to build file-level ref edges
2. Call `engine.ingest(facts)` on first DD-backed query
3. Subsequent queries reuse Warm state

### Eviction

Daemon scheduler calls `engine.evict_if_idle()` on each tick. Default idle_timeout: 30min.

### Config

Add to `src/config.rs`: `dd_idle_timeout_sec: u64` with default `1800`.

## Cargo.toml changes

```toml
differential-dataflow = "0.12"
timely = "0.12"
crossbeam-channel = "0.5"
```

## Files to create/modify

| File | Action |
|------|--------|
| `Cargo.toml` | Add differential-dataflow, timely, crossbeam-channel |
| `src/dd/mod.rs` | Create — public types, re-exports |
| `src/dd/engine.rs` | Create — DdEngine |
| `src/dd/worker.rs` | Create — timely worker thread |
| `src/dd/cycles.rs` | Create — dataflow definition |
| `src/lib.rs` | Add `pub mod dd;` |
| `src/config.rs` | Add `dd_idle_timeout_sec` |
| `tests/dd-test.rs` | Create — 7 tests |

## Test plan

1. `test_cycle_detection_finds_known_cycle` — A→B→C→A, verify cycle found
2. `test_no_cycles_in_dag` — A→B→C, verify empty
3. `test_delta_update_adds_cycle` — start as DAG, add back-edge, verify cycle
4. `test_delta_update_removes_cycle` — start with cycle, remove edge, verify empty
5. `test_eviction_round_trip` — ingest → evict → reingest → query works
6. `test_query_on_cold_engine_errors` — query without ingest returns error
7. `test_public_types_no_dd_deps` — compile-time: Cycle/DdFacts/DdDelta are plain Rust

## Notes

- The dd-spike reference is on branch `spike/hdc-ast-encoding` at `src/bin/dd-spike.rs`. Prefer reimplementing cleanly — only consult if stuck on timely API specifics.
- `differential-dataflow` 0.12 and `timely` 0.12 are the current versions. Check compatibility when adding.
- The worker thread uses `std::thread::spawn` (not tokio) since timely is sync.
- No new database tables needed — DD reads from existing import_edges/refs/symbol_file_map.
