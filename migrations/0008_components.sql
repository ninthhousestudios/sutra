-- Components: durable identity of discovered architectural components.
CREATE TABLE IF NOT EXISTS components (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Component membership: ephemeral — re-derived from clustering on each index.
CREATE TABLE IF NOT EXISTS component_membership (
    component_id TEXT NOT NULL REFERENCES components(id) ON DELETE CASCADE,
    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    PRIMARY KEY (component_id, file_id)
);

-- Semantic anchors: durable — user-selected representative symbols per component.
CREATE TABLE IF NOT EXISTS semantic_anchors (
    id TEXT PRIMARY KEY,
    component_id TEXT NOT NULL REFERENCES components(id) ON DELETE CASCADE,
    symbol_name TEXT NOT NULL,
    rationale TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Component events: ephemeral — architectural change log, re-derived from diffs.
CREATE TABLE IF NOT EXISTS component_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    component_id TEXT NOT NULL REFERENCES components(id) ON DELETE CASCADE,
    event_type TEXT NOT NULL,
    detail TEXT,
    detected_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Aliases: durable — user-defined vocabulary for components.
CREATE TABLE IF NOT EXISTS aliases (
    id TEXT PRIMARY KEY,
    component_id TEXT NOT NULL REFERENCES components(id) ON DELETE CASCADE,
    alias TEXT NOT NULL,
    canonical INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(component_id, alias)
);
