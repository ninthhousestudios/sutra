# Code Review: DD + Constraints + DB — Data Integrity

**Date:** 2026-06-05
**Scope:** DD engine, constraint resolution, DB layer, rules parsing (~5.5k LOC, 13 files)
**Task:** sutra/103
**Verdict:** ship with follow-ups (2 high, 5 medium, 5 low — no critical)

## Verification
- Build: not run (read-only review)
- Tests: not run (read-only review)

## Summary

The DB layer is well-constructed: all queries use parameterized bindings (no SQL injection), JOIN types are correct, WHERE clauses are properly scoped, and ON CONFLICT upserts are well-designed. The DD engine's Mutex-based synchronization and channel protocol are sound — no data races between engine and worker threads.

The main pattern of concern is **missing transactions around DELETE + bulk INSERT** sequences. Three separate methods follow the same anti-pattern: delete all rows, then loop-insert replacements without a wrapping transaction. A crash mid-loop loses data. The DD engine has one subtle differential-dataflow ordering bug in blast_counts, and the constraint system has a user-facing lie (no_cycles scope is parsed but never enforced).

## Findings

```yaml
# ── Data integrity: missing transactions ──────────────────────────────

- id: replace-health-findings-no-txn
  severity: high
  category: data-integrity
  title: replace_health_findings DELETE + bulk INSERT not in a transaction
  location: src/db/health.rs:96-122
  evidence: |
    conn.execute("DELETE FROM health_findings", [])?;
    let mut stmt = conn.prepare("INSERT INTO health_findings ...")?;
    for f in findings { stmt.execute(...)?; }
    No BEGIN/COMMIT wrapping. Each statement auto-commits.
  why: |
    If the process crashes or errors partway through the insert loop, the table
    is left partially populated — some findings are lost with no way to detect
    the inconsistency. Every other "replace all" pattern in the codebase
    (replace_all_anchors, replace_all_aliases, replace_pattern_families) wraps
    the delete+insert in a transaction.
  recommendation: |
    Wrap in conn.unchecked_transaction(), matching the pattern in
    replace_all_anchors (components.rs:242).
  confidence: high

- id: replace-hrr-vectors-no-txn
  severity: high
  category: data-integrity
  title: replace_hrr_vectors DELETE + bulk INSERT not in a transaction
  location: src/db/similarity.rs:74-84
  evidence: |
    conn.execute("DELETE FROM hrr_vectors", [])?;
    let mut stmt = conn.prepare("INSERT INTO hrr_vectors ...")?;
    for &(sym_id, mode, blob) in vectors { stmt.execute(...)?; }
    Same non-transactional pattern as replace_health_findings.
  why: |
    Crash mid-loop loses all vector data. Similarity queries would silently
    return empty/incomplete results until the next full rebuild.
  recommendation: |
    Wrap in conn.unchecked_transaction().
  confidence: high

- id: delete-file-cascade-no-txn
  severity: medium
  category: data-integrity
  title: delete_file_cascade FTS5 cleanup and file deletion not transactional
  location: src/db/mod.rs:435-454
  evidence: |
    // Manual FTS5 sync: delete every symbol FTS row for this file.
    for sid in &symbol_ids {
        conn.execute("DELETE FROM symbols_fts WHERE symbol_id = ?1", ...)?;
    }
    conn.execute("DELETE FROM files WHERE id = ?1", ...)?;
    Two logically coupled operations, no wrapping transaction.
  why: |
    If the process crashes between FTS5 deletes and the file delete (or vice
    versa), the database has orphaned FTS5 rows or missing FTS entries for
    existing symbols. FTS search results become inconsistent.
  recommendation: |
    Wrap in conn.unchecked_transaction().
  confidence: high

# ── DD engine correctness ─────────────────────────────────────────────

- id: blast-counts-order-dependent
  severity: medium
  category: correctness
  title: blast_counts inspect callback produces wrong results depending on batch record order
  location: src/constraints/worker.rs:136-143
  evidence: |
    .inspect(move |((dst, count), _time, diff)| {
        let mut map = blast_counts_inspect.borrow_mut();
        if *diff > 0 { map.insert(*dst, *count); }
        else if *diff < 0 { map.remove(dst); }
    })
    When count_total() emits a batch with retraction (-1 for old count) and
    insertion (+1 for new count), processing order is not guaranteed by
    differential dataflow. If retraction arrives after insertion, the entry
    is removed entirely — node disappears from blast_counts when it should
    have a new count value.
  why: |
    blast_radius queries return 0 or "not found" for nodes whose actual
    blast count was updated in the same batch. The cycle_nodes inspect
    (lines 122-128) doesn't have this bug because it's a set (insert/remove
    are idempotent for presence), but blast_counts maps to a value.
  recommendation: |
    Guard the removal: only remove if the stored count matches the retracted
    count. If map.get(dst) == Some(count), remove; otherwise the newer
    value was already inserted and the retraction is stale.
  confidence: high

- id: reload-discards-forbidden-pairs
  severity: medium
  category: correctness
  title: reload() silently discards forbidden_pairs
  location: src/constraints/engine.rs:67-74
  evidence: |
    reload() unconditionally resets state to Loaded with empty forbidden
    pairs. If set_forbidden_pairs() was called before reload(), all pairs
    are lost. The next query_violations() returns zero violations even
    though constraint rules haven't changed.
  why: |
    After an index rebuild triggers reload(), constraint violations
    silently disappear until the caller re-calls set_forbidden_pairs.
    No API signal that re-setting is required. In contrast, evict_if_idle
    (line 326-338) correctly preserves forbidden_pairs.
  recommendation: |
    Read the existing forbidden_pairs from the current state before
    replacing it in reload(), or return a signal that pairs were cleared.
  confidence: high

# ── Constraint resolution ─────────────────────────────────────────────

- id: no-cycles-scope-not-enforced
  severity: medium
  category: correctness
  title: no_cycles scope is parsed and hashed but never used to filter cycles
  location: src/tools/constraints.rs:192-196, src/tools/orient.rs:218-220, src/tools/review.rs:442-444
  evidence: |
    let no_cycles_constraint = all_constraints.iter()
        .find(|c| matches!(c.kind, ConstraintKind::NoCycles));
    for cycle in engine.query_cycles()? { ... }
    
    query_cycles() returns ALL cycles in the entire graph. The scope field
    on the constraint (e.g., scope = "src/core/") is accepted, hashed into
    the constraint identity, but never used to filter results.
  why: |
    Users can write scope on no_cycles constraints and believe they're
    scoping the check. All three call sites (.find() picks the first
    NoCycles constraint) attribute every cycle to that constraint. If
    multiple NoCycles constraints exist with different scopes, all but
    the first are dead — their metadata (severity, provenance) is unused.
  recommendation: |
    Either filter query_cycles() results to only include cycles where
    participating files fall within the constraint's scope, or reject/warn
    when scope is provided on no_cycles (and remove it from the identity hash).
  confidence: high

# ── SQL correctness ───────────────────────────────────────────────────

- id: create-waiver-wrong-rowid
  severity: medium
  category: correctness
  title: create_waiver returns wrong ID on ON CONFLICT DO UPDATE path
  location: src/db/conventions.rs:399-411
  evidence: |
    conn.execute("INSERT INTO convention_waivers ... ON CONFLICT ... DO UPDATE SET ...")?;
    Ok(conn.last_insert_rowid())
    
    SQLite's last_insert_rowid() does NOT return the rowid of an updated
    row on the ON CONFLICT DO UPDATE path. It returns the rowid of the most
    recent successful INSERT, which could be from a prior call or a
    different table entirely.
  why: |
    When updating an existing waiver (same convention_id + symbol +
    component), the caller receives a stale/wrong waiver ID. The companion
    create_health_waiver in health.rs handles this correctly with a
    follow-up SELECT.
  recommendation: |
    After execute, do a SELECT to fetch the actual ID:
    SELECT id FROM convention_waivers WHERE convention_id=?1 AND
    symbol_qualified_name=?2 AND component_id=?3
  confidence: high

- id: get-convention-state-swallows-errors
  severity: low
  category: correctness
  title: get_convention_state .ok() swallows all query errors, not just "no rows"
  location: src/db/conventions.rs:192-214
  evidence: |
    let row = conn.query_row(...).ok();
    Ok(row)
    
    .ok() converts QueryReturnedNoRows AND real DB errors (corruption,
    locked, etc.) into None. Compare with clustering_meta (components.rs)
    which correctly matches QueryReturnedNoRows vs other errors.
  why: |
    A locked or corrupted DB silently returns None instead of surfacing
    the error. Same pattern in get_proposal (conventions.rs:330-351).
  recommendation: |
    Match on the error variant:
    Err(QueryReturnedNoRows) => Ok(None),
    Err(e) => Err(e.into()),
  confidence: high

- id: codebook-entries-wrong-count
  severity: low
  category: correctness
  title: save_hrr_codebook_entries return value misreports insert count
  location: src/db/similarity.rs:99-110
  evidence: |
    INSERT OR IGNORE INTO hrr_codebook ...
    Ok(entries.len())  // returns attempted count, not actual inserts
  why: |
    INSERT OR IGNORE silently skips duplicates. Callers relying on the
    return value for progress tracking get an inflated count.
  recommendation: |
    Accumulate actual inserts via conn.changes() or stmt.execute() return value.
  confidence: high

- id: health-score-type-mismatch
  severity: low
  category: schema
  title: health_score column declared INTEGER in schema but used as f64 in Rust
  location: src/db/mod.rs:199 vs migration 0003
  evidence: |
    Migration: ALTER TABLE snapshots ADD COLUMN health_score INTEGER NOT NULL DEFAULT 0
    Rust: health_score: f64 in both SnapshotRow and SnapshotParams
    Works due to SQLite type affinity coercion, but schema is misleading.
  why: |
    Anyone querying the DB directly or adding CHECK constraints would
    assume integer values. No runtime bug due to SQLite's flexibility.
  recommendation: |
    Document the mismatch. Not worth a migration (SQLite doesn't support
    ALTER COLUMN), but note for future schema documentation.
  confidence: high

- id: snapshot-bulk-insert-no-txn
  severity: low
  category: data-integrity
  title: insert_snapshot_files and insert_snapshot_components lack transactions
  location: src/db/mod.rs:1166-1211
  evidence: |
    Both functions loop-insert without a wrapping transaction. The companion
    insert_snapshot_atomic correctly wraps the same work in a transaction,
    suggesting these are vestigial or an oversight.
  why: |
    Same crash-consistency concern as the high-severity findings, but
    lower impact since snapshot data is derivable. Also a performance
    issue — SQLite auto-commits each INSERT to WAL without an explicit
    transaction.
  recommendation: |
    Wrap in unchecked_transaction(), or remove in favor of insert_snapshot_atomic.
  confidence: high

- id: recursive-dfs-stack-depth
  severity: low
  category: robustness
  title: Recursive DFS in Kosaraju's SCC can stack overflow on deep graphs
  location: src/constraints/worker.rs:317-349
  evidence: |
    dfs_finish and dfs_collect use unbounded recursion. A pathological
    graph with ~100K+ cycle-participating nodes in a single chain would
    overflow the default 8MB thread stack.
  why: |
    Unlikely in real codebases (cycle-participating node counts are small),
    but the worker thread uses the default stack size.
  recommendation: |
    Convert to iterative DFS with explicit Vec stack. Low priority.
  confidence: medium
```

## Categories with no issues

- **SQL injection**: All queries use parameterized bindings. `format!()` into SQL appears only for compile-time constants (migration savepoint names, PRAGMA table names, TABLE_REGISTRY entries) — never user input.
- **Missing WHERE clauses**: All UPDATEs and DELETEs are correctly scoped. Table-wide DELETEs (like `DELETE FROM commit_files`) are intentional replace-all operations inside transactions.
- **JOIN types**: LEFT JOINs correctly used for optional relationships (dead-symbol detection, convention state). INNER JOINs correct elsewhere.
- **ON CONFLICT behavior**: Upserts in `upsert_file` and `create_constraint_waiver` correctly update only intended columns.
- **Migration completeness**: All tables and columns referenced in queries exist in the migration chain. Migration ordering is correct — no forward references.
- **DD thread synchronization**: Sound. Single `Mutex<DdState>` serializes all operations. Crossbeam channels for worker communication. `AtomicBool` uses correct Release/Acquire ordering.
- **DD state machine**: Three-state (Cold/Loaded/Warm) with complete coverage. No unreachable states or missing transitions.
- **Forbidden-pair queries**: Semijoin approach is correct for the defined semantics (direct edges). Updates maintained incrementally.
- **TOML parsing**: Missing fields produce clear errors. Wrong types cause serde deserialization errors that propagate correctly.
- **Constraint identity**: blake3 with null-byte domain separators prevents collision from concatenation ambiguity.
