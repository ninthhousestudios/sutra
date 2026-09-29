-- Drop FCA convention detection (sutra/518). Conventions were mined on every
-- parse and persisted here for the sutra_conventions list tool, the last
-- reader; that tool is retired, so nothing reads or writes these tables.
--
-- ephemeral_only: both tables are Ephemeral. On reindex, 0005 and 0040
-- recreate them before this replays and drops them again.
DROP TABLE IF EXISTS conventions;
DROP TABLE IF EXISTS fca_cache;
