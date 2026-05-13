CREATE TABLE IF NOT EXISTS conventions (
    id         TEXT    NOT NULL PRIMARY KEY,
    antecedent TEXT    NOT NULL,
    consequent TEXT    NOT NULL,
    support    INTEGER NOT NULL,
    confidence REAL    NOT NULL,
    first_seen TEXT    NOT NULL DEFAULT (datetime('now')),
    last_seen  TEXT    NOT NULL DEFAULT (datetime('now')),
    suppressed INTEGER NOT NULL DEFAULT 0
);
