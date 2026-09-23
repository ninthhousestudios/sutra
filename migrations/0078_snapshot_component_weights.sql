-- Persist component member weights and the instability penalty (sutra/436).
--
-- A component score is the line-count-weighted mean of its member scores minus
-- an instability penalty. The component basis (0077) covers membership and
-- member bases but not weights, so a comment-only edit that changes a member's
-- line count moved the component score and trend reported it as a measured
-- change. Trend now measures a component delta at the baseline's member weights
-- and reports the weight/mix shift separately, which needs both snapshots'
-- weights and penalties.
--
-- health_snapshot_components:
--   weights_recorded     1 when the member weights below were written. 0 on every
--                        row before sutra/436: weights Unknown, so the component
--                        delta is incomparable — never measured at a defaulted
--                        weight.
--   instability_penalty  the penalty subtracted from the weighted mean. NULL =
--                        unknown (instability failed, or legacy row).
-- health_snapshot_component_members: one row per (component, member file) with
--                        the aggregation weight (files.line_count) it was scored
--                        at. A file may belong to several components.
-- Ephemeral (the snapshot detail tables are dropped and recreated on reindex) —
-- replay to re-add the columns and recreate the table.
ALTER TABLE health_snapshot_components ADD COLUMN weights_recorded INTEGER NOT NULL DEFAULT 0;
ALTER TABLE health_snapshot_components ADD COLUMN instability_penalty REAL;
CREATE TABLE IF NOT EXISTS health_snapshot_component_members (
    snapshot_id  INTEGER NOT NULL,
    component_id TEXT    NOT NULL,
    file_path    TEXT    NOT NULL,
    weight       INTEGER NOT NULL,
    PRIMARY KEY (snapshot_id, component_id, file_path)
);
