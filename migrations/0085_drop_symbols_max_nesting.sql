-- Drop symbols.max_nesting (sutra/474). Its only reader was the
-- nested_complexity biomarker, removed with the other biomarkers (0083); the
-- parsers no longer compute it.
--
-- ephemeral_only: symbols is Ephemeral. On reindex, 0001 recreates it and 0027
-- re-adds the column before this replays and drops it again.
ALTER TABLE symbols DROP COLUMN max_nesting;
