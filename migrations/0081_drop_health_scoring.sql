-- Drop the health scoring, trend and run-publication layer (sutra/473; decision
-- in docs/health-disposition.md).
--
-- health_runs / health_current published per-(file, producer) outcomes so scores
-- stayed comparable over time; the health_snapshot_* tables and the snapshots
-- health_score / health_run_id columns recorded those scores per checkpoint for
-- sutra_trend. Scores, trend and the demand refresh are gone, so nothing reads
-- or writes them. The snapshots table itself stays: it is the parse record
-- behind last_parse_time / last_parse_info.
--
-- ephemeral_only: every object here is Ephemeral. On reindex, 0001/0003/0033/
-- 0075/0077 recreate them before this replays and drops them again.
DROP TABLE IF EXISTS health_current;
DROP TABLE IF EXISTS health_runs;
DROP TABLE IF EXISTS health_snapshot_component_members;
DROP TABLE IF EXISTS health_snapshot_components;
DROP TABLE IF EXISTS health_snapshot_files;
ALTER TABLE snapshots DROP COLUMN health_score;
ALTER TABLE snapshots DROP COLUMN health_run_id;
