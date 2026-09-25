-- Drop the health biomarker findings (sutra/474; decision in
-- docs/health-disposition.md).
--
-- health_findings held the persistent biomarker output rebuilt on every parse;
-- health_coverage has had no reader or writer since sutra/416. The producers
-- are gone, so nothing reads or writes either table.
--
-- ephemeral_only: both tables are Ephemeral. On reindex, 0027/0072 recreate
-- them before this replays and drops them again.
DROP TABLE IF EXISTS health_findings;
DROP TABLE IF EXISTS health_coverage;
