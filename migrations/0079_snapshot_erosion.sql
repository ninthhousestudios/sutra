-- Workspace erosion aggregates on checkpoints (sutra/442).
--
-- eroded_mass / total_mass  summed cognitive × sqrt(sloc) over outermost,
--                non-test functions (health::erosion). Standalone metric, not a
--                health biomarker.
-- erosion_version  health::erosion::EROSION_VERSION the values were computed
--                under; trend compares erosion only across equal versions.
-- All NULLABLE on purpose: unlike total_complexity (NOT NULL DEFAULT 0), a
-- checkpoint written before the metric existed must read as unknown, not 0.
-- Ephemeral ALTER (snapshots is dropped and recreated on reindex) — replay to
-- re-add the columns.
ALTER TABLE snapshots ADD COLUMN eroded_mass REAL;
ALTER TABLE snapshots ADD COLUMN total_mass REAL;
ALTER TABLE snapshots ADD COLUMN erosion_version INTEGER;
