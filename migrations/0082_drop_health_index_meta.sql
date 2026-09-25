-- Drop the health-only index_meta columns (sutra/473).
--
-- index_epoch keyed retained health runs to one index lifetime; the runs are
-- gone (0081). git_availability has had no reader or writer since sutra/416.
--
-- NOT ephemeral_only: index_meta is Durable and reindex does not drop it, so
-- 0071/0074 never replay and this must not either.
ALTER TABLE index_meta DROP COLUMN index_epoch;
ALTER TABLE index_meta DROP COLUMN git_availability;
