-- Drop the snapshots erosion columns (sutra/475; decision in
-- docs/health-disposition.md). The erosion metric and the review
-- erosion_delta block are gone, so nothing reads or writes them. The
-- cognitive >= 15 risk gate lives on in diff_impact from per-symbol cognitive.
--
-- ephemeral_only: snapshots is Ephemeral. On reindex, 0001 recreates it and
-- 0079 re-adds the columns before this replays and drops them again.
ALTER TABLE snapshots DROP COLUMN eroded_mass;
ALTER TABLE snapshots DROP COLUMN total_mass;
ALTER TABLE snapshots DROP COLUMN erosion_version;
