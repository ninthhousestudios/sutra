CREATE TABLE IF NOT EXISTS health_waivers (
    id                    INTEGER PRIMARY KEY AUTOINCREMENT,
    biomarker_kind        TEXT NOT NULL,
    file_path             TEXT NOT NULL,
    symbol_qualified_name TEXT,
    rationale             TEXT NOT NULL,
    waived_by             TEXT NOT NULL,
    created_at            TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at            TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_health_waivers_unique
    ON health_waivers (biomarker_kind, file_path, COALESCE(symbol_qualified_name, ''));
