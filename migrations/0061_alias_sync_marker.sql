-- Freshness marker for the .sutra/aliases.toml projection (sutra/320). The
-- `aliases` table (0012) is a projection of that tracked file, but its only
-- writer runs inside parse_workspace — so a workspace that skips reparse
-- (frozen index, or a fresh index whose source didn't change) never re-syncs
-- alias edits, leaving forward name->symbol resolution dead. This single row
-- stores a content hash of the last file the `aliases` table was projected
-- from, so startup can cheaply detect a stale projection after a hand-edit,
-- git pull, or branch switch and re-sync without a full parse.
CREATE TABLE IF NOT EXISTS alias_sync (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    file_hash TEXT                       -- NULL = never projected (always stale)
);

INSERT OR IGNORE INTO alias_sync (id, file_hash) VALUES (1, NULL);
