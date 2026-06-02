CREATE TABLE IF NOT EXISTS convention_templates (
    convention_id TEXT NOT NULL PRIMARY KEY,
    template_text TEXT NOT NULL,
    exemplar_symbols TEXT NOT NULL,
    generated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
