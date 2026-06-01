CREATE TABLE IF NOT EXISTS component_clustering_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    edge_count INTEGER NOT NULL DEFAULT 0,
    file_count INTEGER NOT NULL DEFAULT 0,
    clustered_at TEXT NOT NULL DEFAULT (datetime('now'))
);
