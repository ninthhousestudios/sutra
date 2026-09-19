-- Persist per-file analysis completeness in health snapshots (sutra/408).
--
-- ScoredFile.missing (the worst-cased biomarkers that flag a score `partial`)
-- was computed but discarded when writing health_snapshot_files, so trend could
-- not tell partial analysis from real degradation: a file worst-cased to 5.0
-- because its analysis was incomplete looked identical to a file that genuinely
-- earned 5.0. These columns carry that completeness through the snapshot.
--
-- `partial` is 1 when the score was computed from incomplete analysis;
-- `missing_biomarkers` is a JSON array of the worst-cased biomarker names.
-- Ephemeral ALTER (health_snapshot_files is dropped and recreated on reindex by
-- 0033) — replays after 0033 to re-add the columns.
ALTER TABLE health_snapshot_files ADD COLUMN partial INTEGER NOT NULL DEFAULT 0;
ALTER TABLE health_snapshot_files ADD COLUMN missing_biomarkers TEXT NOT NULL DEFAULT '[]';
