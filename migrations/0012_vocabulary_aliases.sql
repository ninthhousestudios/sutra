DROP TABLE IF EXISTS aliases;
CREATE TABLE IF NOT EXISTS aliases (
    id TEXT PRIMARY KEY,
    term TEXT NOT NULL UNIQUE,
    target_kind TEXT NOT NULL CHECK(target_kind IN ('component', 'file', 'symbol')),
    target_ref TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
