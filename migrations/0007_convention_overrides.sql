CREATE TABLE IF NOT EXISTS convention_overrides (
    convention_id    TEXT NOT NULL PRIMARY KEY,
    lifecycle_state  TEXT NOT NULL DEFAULT 'descriptive',
    override_reason  TEXT,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Migrate suppressed conventions
INSERT OR IGNORE INTO convention_overrides (convention_id, lifecycle_state, override_reason)
SELECT id, 'forbidden', 'migrated from suppressed flag'
FROM conventions WHERE suppressed = 1;

-- Rebuild conventions without suppressed column
ALTER TABLE conventions RENAME TO conventions_old;

CREATE TABLE conventions (
    id         TEXT    NOT NULL PRIMARY KEY,
    antecedent TEXT    NOT NULL,
    consequent TEXT    NOT NULL,
    support    INTEGER NOT NULL,
    confidence REAL    NOT NULL,
    first_seen TEXT    NOT NULL DEFAULT (datetime('now')),
    last_seen  TEXT    NOT NULL DEFAULT (datetime('now'))
);

INSERT INTO conventions (id, antecedent, consequent, support, confidence, first_seen, last_seen)
SELECT id, antecedent, consequent, support, confidence, first_seen, last_seen
FROM conventions_old;

DROP TABLE conventions_old;
