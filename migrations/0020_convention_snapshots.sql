CREATE TABLE IF NOT EXISTS convention_snapshots (
    id                          INTEGER PRIMARY KEY AUTOINCREMENT,
    component_id                TEXT    NOT NULL,
    snapshot_ts                 TEXT    NOT NULL DEFAULT (datetime('now')),
    entropy                     REAL    NOT NULL,
    symbol_count                INTEGER NOT NULL,
    attribute_distribution      TEXT    NOT NULL,
    attribute_distribution_hash TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_conv_snapshots_component_ts
    ON convention_snapshots(component_id, snapshot_ts DESC);
