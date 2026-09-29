-- Drop pattern families (sutra/519). Families were recomputed on every parse
-- that changed HRR vectors and read only by sutra_similar's no-symbol mode,
-- now removed; the snapshot count was written but never surfaced.
--
-- ephemeral_only: all three are Ephemeral. On reindex, 0031/0057 recreate the
-- tables and 0032 re-adds the column before this replays and drops them again.
DROP TABLE IF EXISTS pattern_family_members;
DROP TABLE IF EXISTS pattern_families;
ALTER TABLE snapshots DROP COLUMN pattern_family_count;
