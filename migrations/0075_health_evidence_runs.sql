-- Immutable health evidence runs and the current-run pointer (sutra/414).
--
-- A run records, for one health computation, the input identity it was computed
-- against (input_stamp), the per-producer/per-file outcomes, and the retained
-- findings — all as JSON blobs. The blobs are deliberately FK-free: the health
-- evidence contract (sutra/412) requires immutable evidence that does NOT cascade
-- through live file/symbol foreign keys, and whose captured path/symbol labels
-- (inside the blobs) are authoritative for display while any numeric id is
-- diagnostic only. validate() compares input_stamp in Rust; SQL never inspects
-- it.
--
-- Runs are insert-only; publication advances the health_current pointer
-- atomically in the same transaction (Db::publish_health_run), which also
-- verifies data_generation has not moved since the inputs were observed.
--
-- Ephemeral: like health_findings/coverage/snapshots, a full parse rebuilds these
-- from scratch, and a legacy index simply has zero runs — readers then return
-- None, which the contract requires to be treated as LegacyUnknown, never
-- backfilled from content hashes / HEAD / snapshot completeness. ephemeral_only:
-- reindex drops these tables, so the CREATEs must replay to recreate them.
CREATE TABLE IF NOT EXISTS health_runs (
    run_id           INTEGER PRIMARY KEY AUTOINCREMENT,
    index_epoch      TEXT    NOT NULL,
    graph_generation INTEGER NOT NULL,
    created_at       TEXT    NOT NULL,
    input_stamp      TEXT    NOT NULL,
    outcomes         TEXT    NOT NULL,
    findings         TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS health_current (
    id          INTEGER PRIMARY KEY CHECK (id = 1),
    run_id      INTEGER NOT NULL REFERENCES health_runs(run_id),
    index_epoch TEXT    NOT NULL
);
