CREATE TABLE IF NOT EXISTS convention_waivers (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    convention_id         TEXT NOT NULL,
    symbol_qualified_name TEXT NOT NULL,
    component_id          TEXT NOT NULL DEFAULT '',
    rationale             TEXT NOT NULL,
    waived_by             TEXT NOT NULL,
    waived_at             TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(convention_id, symbol_qualified_name, component_id)
);
