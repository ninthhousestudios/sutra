ALTER TABLE files ADD COLUMN needs_resolution BOOLEAN NOT NULL DEFAULT 0;

-- Existing files need a full resolution pass on upgrade.
UPDATE files SET needs_resolution = 1;

CREATE TABLE IF NOT EXISTS index_meta (
    id                          INTEGER PRIMARY KEY CHECK (id = 1),
    data_generation             INTEGER NOT NULL DEFAULT 0,
    derived_complete_generation INTEGER NOT NULL DEFAULT 0
);

-- Mismatch generations when files exist so the first post-upgrade parse
-- forces full recovery instead of hitting the fast path.
INSERT OR IGNORE INTO index_meta (id, data_generation, derived_complete_generation)
VALUES (1, (SELECT CASE WHEN COUNT(*) > 0 THEN 1 ELSE 0 END FROM files), 0);
