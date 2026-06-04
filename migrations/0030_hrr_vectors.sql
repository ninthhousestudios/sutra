CREATE TABLE IF NOT EXISTS hrr_vectors (
    symbol_id  INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    mode       TEXT NOT NULL,
    vector     BLOB NOT NULL,
    PRIMARY KEY (symbol_id, mode)
);
