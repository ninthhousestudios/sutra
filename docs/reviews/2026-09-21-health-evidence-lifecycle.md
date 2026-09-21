# Sutra/411: health evidence lifecycle review

Reviewed commit: `6ca78d7aa1c307d762270ef4fa12cddabc443eb7`.

The immediate root cause in 411 is correct. Its recorded implementation decision
is incomplete and should not be implemented verbatim. Removing session-start
reparse is not a requirement; Josh explicitly questioned that premise on
2026-09-21. Preserve it while repairing health freshness independently.

## Verified failure mechanism

1. `Db::replace_file_data` deletes the existing `files` row, cascading away
   `health_findings`, `health_coverage`, and `commit_files` (migrations 0027,
   0072, 0025). This is also used by full parsing, whose later health rebuild
   normally masks the destruction.
2. `parse_incremental` refreshes extraction and resolution, but neither health
   findings nor snapshots. It advances content fingerprints, leaving derived
   data behind.
3. `compute_health_delta` trusts the previous snapshot's completeness to score
   current findings. An old complete snapshot therefore licenses the now-empty
   finding set as clean. The comment claiming every parse snapshots is false.
4. Absolute scoring detects the missing coverage for the edited file and
   worst-cases it. The same edit can therefore look worse in absolute health
   and better in review deltas. Neither represents newly observed code quality.
5. Startup checks content drift and parser stamp, not health completeness. Once
   incremental refresh finishes, restarting does not repair the missing health.
   `Db::has_pending_work` already recognizes the derived-generation gap, so an
   explicit full parse can rebuild even when source bytes are unchanged.

This is an evidence ownership and invalidation problem. Parser freshness and
health freshness are different contracts. Merely preserving findings would
replace missing evidence with potentially stale evidence; merely adding a
coverage flag leaves health permanently pessimistic without a refresh path.

## Executed reproduction

A temporary integration probe ran the actual `parse_workspace` and
`parse_incremental` paths on an isolated two-file workspace. A deeply nested
function produced a complete snapshot; a comment-only edit then triggered
incremental refresh. Commit-file links were seeded for both files separately
to isolate their deletion from git ingestion.

`cargo test --test issue_411_probe -- --nocapture` reported:

```text
before_findings=1 after_findings=0 recomputed_findings=1 baseline=8.66
fresh=true remaining_history=1 improved=1 current=10
test result: ok. 1 passed; 0 failed
```

The probe asserts the existing bug, not the desired behavior. Its source was
moved to `/tmp/sutra-411-probe.rs` after execution; it is not a permanent
regression test. The production fix must add tests asserting correct behavior.
The separate history result proves link destruction, not the entire git-debt
scoring scenario; that still needs the acceptance test specified by 411.

## Corrections to the proposed model

| Evidence / finding | Actual dependencies |
|---|---|
| Raw commit-file evidence | Repository history, configured rolling window, ingestion success, mapping to indexed paths |
| Nested complexity | File extraction and parser identity |
| Import-cycle membership | Resolved workspace import graph, including other files |
| Dead-code ratio | Symbol/reference state |
| Co-change scatter / change entropy | Ingested history and indexed file population |
| Ownership risk | History plus `.sutra/owners.toml` aliases |
| Hidden coupling | History **and current static edges** |
| Blast-radius churn | History **and current graph rollups** |

`compute_hidden_coupling` explicitly suppresses co-changing pairs with a static
edge: it is not a pure-history biomarker. `compute_blast_radius_churn` reads
`files.blast_radius`; rerunning its producer after incremental resolution alone
does not refresh that rollup. A file content hash cannot validate graph findings
on unchanged files when another file changes. HEAD alone cannot validate a
rolling `git log --since` window or ownership configuration.

Preserving raw history is sound. Preserving all git-labelled findings as current
until HEAD changes is not. Stable identity alone also does not solve invalidation.

## Recommended implementation direction (not yet implemented)

1. Preserve file identity across content replacement, or decouple history from
   ephemeral extraction IDs. Prefer updating the existing file row and replacing
   extraction children after auditing every child-table lifecycle. Keep actual
   file deletion distinct from content replacement. Simply dropping the FK is
   insufficient: it leaves orphaned or misassociated evidence.
2. Separate health freshness from parser freshness. Start conservatively with
   workspace graph generations for graph-dependent analysis, plus explicit
   history/configuration validity. Use per-producer dependency stamps where
   selective reuse warrants them; do not force everything into two exclusive
   structural/git buckets. Existing global derived completeness includes many
   unrelated products and must not be marked complete by a health-only rebuild.
3. Extract a shared health refresh operation with its required inputs (including
   rollups). Full parse and health consumers should use the same producers.
   Prefer refreshing when health/review is requested, under the parse coordinator,
   to putting all health work on every symbol query. This placement is a design
   recommendation, not a measured performance conclusion. Benchmark before
   choosing eager graph recomputation on the incremental hot path.
4. Publish findings and successful coverage atomically for a coherent generation.
   If refresh fails or is deferred, report missing/stale analysis explicitly;
   retained old evidence must carry its provenance and cannot claim currentness.
5. Remove snapshot-derived authorization of live findings. Compare like-for-like
   score bases and expose completeness on both sides. A worst-cased missing
   analysis score is a conservative bound, not measured degradation. On-demand
   attribution and temporal baseline-health change are distinct comparisons;
   do not force their deltas to wash by pretending evidence survived.
6. Keep startup reparse. It may also opportunistically repair a known health
   backlog, but cannot be the only repair path for a live session or HTTP client.
7. Fix git probe failure/absence classification and trend completeness alongside
   this work, as separate logical changes. These are independent defects, not
   all consequences of the file-row cascade.

Keep the task's types-first gate for implementation. Expand its real-path tests
to cover unchanged files affected by graph edits, hidden-coupling changes with
unchanged HEAD, stale blast-radius rollups, configuration/window invalidation,
and same-content explicit recovery, in addition to the existing four scenarios.

## Prior reasoning to retire

408 added a useful safety bound but did not restore analysis. 409 attempted to
preserve numerical comparability based on the false premise that every parse
records a snapshot and retains its findings. Its synthetic score tests did not
exercise the destructive incremental path. The snapshot-mirroring lesson
`01a0bb18-3deb-721c-8e57-2da781908e59` was anti-verified during this review.
Do not use its rule as implementation guidance.
