ALTER TABLE files ADD COLUMN needs_resolution BOOLEAN NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS index_meta (
    id                          INTEGER PRIMARY KEY CHECK (id = 1),
    data_generation             INTEGER NOT NULL DEFAULT 0,
    derived_complete_generation INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO index_meta (id, data_generation, derived_complete_generation)
VALUES (1, 0, 0);
