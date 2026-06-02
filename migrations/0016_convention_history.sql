CREATE TABLE IF NOT EXISTS convention_history (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    convention_id TEXT    NOT NULL,
    support       INTEGER NOT NULL,
    confidence    REAL    NOT NULL,
    snapshot_id   TEXT    NOT NULL,
    recorded_at   TEXT    NOT NULL DEFAULT (datetime('now')),
    FOREIGN KEY (convention_id) REFERENCES conventions(id) ON DELETE CASCADE
);

CREATE INDEX idx_convention_history_cid ON convention_history(convention_id, recorded_at);
CREATE INDEX idx_convention_history_snapshot ON convention_history(snapshot_id);
