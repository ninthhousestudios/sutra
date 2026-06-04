ALTER TABLE symbols ADD COLUMN max_nesting INTEGER;

CREATE TABLE IF NOT EXISTS health_findings (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id           INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    symbol_id         INTEGER REFERENCES symbols(id) ON DELETE CASCADE,
    biomarker_kind    TEXT NOT NULL,
    severity          TEXT NOT NULL,
    confidence        REAL NOT NULL,
    provenance        TEXT NOT NULL DEFAULT 'computed',
    metric_value      REAL NOT NULL,
    threshold         REAL NOT NULL,
    detail            TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_health_findings_file ON health_findings(file_id);
CREATE INDEX IF NOT EXISTS idx_health_findings_kind ON health_findings(biomarker_kind);
