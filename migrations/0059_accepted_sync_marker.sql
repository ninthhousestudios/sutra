-- Freshness marker for the portable acceptance file (.sutra/accepted.toml,
-- sutra/303). That tracked file is the source of truth for constraint waivers
-- and instance acks; the constraint_waivers / constraint_instance_acks tables
-- are a projection rebuilt from it. This single row stores a content hash of the
-- last file the cache was projected from, so any reader (server or guard) can
-- detect a stale cache after a hand-edit, git pull, or branch switch and
-- re-project (server) or resolve in-memory (guard) instead of trusting stale
-- rows.
CREATE TABLE IF NOT EXISTS accepted_sync (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    file_hash TEXT                       -- NULL = never projected (always stale)
);

INSERT OR IGNORE INTO accepted_sync (id, file_hash) VALUES (1, NULL);
