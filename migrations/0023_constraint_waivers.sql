CREATE TABLE IF NOT EXISTS constraint_waivers (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    constraint_id         TEXT NOT NULL,
    constraint_name       TEXT,
    file_path             TEXT NOT NULL,
    symbol_qualified_name TEXT,
    rationale             TEXT NOT NULL,
    waived_by             TEXT NOT NULL,
    created_at            TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at            TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_constraint_waivers_unique
    ON constraint_waivers (constraint_id, file_path, COALESCE(symbol_qualified_name, ''));
