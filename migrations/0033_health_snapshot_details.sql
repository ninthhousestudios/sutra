CREATE TABLE IF NOT EXISTS health_snapshot_files (
    snapshot_id     INTEGER NOT NULL,
    file_id         INTEGER NOT NULL,
    file_path       TEXT    NOT NULL,
    score           REAL    NOT NULL,
    category_scores TEXT    NOT NULL DEFAULT '{}',
    PRIMARY KEY (snapshot_id, file_id)
);

CREATE INDEX IF NOT EXISTS idx_hsf_path_snapshot
    ON health_snapshot_files(file_path, snapshot_id);

CREATE TABLE IF NOT EXISTS health_snapshot_components (
    snapshot_id    INTEGER NOT NULL,
    component_id   TEXT    NOT NULL,
    component_name TEXT    NOT NULL,
    score          REAL    NOT NULL,
    member_count   INTEGER NOT NULL,
    total_nloc     INTEGER NOT NULL,
    PRIMARY KEY (snapshot_id, component_id)
);
