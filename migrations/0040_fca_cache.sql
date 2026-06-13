CREATE TABLE IF NOT EXISTS fca_cache (
    id           INTEGER PRIMARY KEY CHECK(id = 1),
    matrix_hash  BLOB NOT NULL
);
