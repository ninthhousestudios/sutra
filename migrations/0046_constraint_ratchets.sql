CREATE TABLE IF NOT EXISTS constraint_ratchets (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    constraint_id        TEXT NOT NULL UNIQUE,
    name                 TEXT,
    rendered_description TEXT NOT NULL,
    severity_floor       TEXT NOT NULL,
    registered_at        TEXT NOT NULL DEFAULT (datetime('now')),
    released_at          TEXT,
    released_by          TEXT,
    release_rationale    TEXT
);
