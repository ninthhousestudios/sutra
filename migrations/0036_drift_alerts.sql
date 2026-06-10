CREATE TABLE IF NOT EXISTS drift_alerts (
    id                   INTEGER PRIMARY KEY AUTOINCREMENT,
    component_id         TEXT NOT NULL,
    component_name       TEXT NOT NULL,
    entropy_old          REAL NOT NULL,
    entropy_new          REAL NOT NULL,
    delta                REAL NOT NULL,
    diverging_attributes TEXT NOT NULL
);
