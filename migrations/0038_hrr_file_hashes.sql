CREATE TABLE IF NOT EXISTS hrr_file_hashes (
    file_id       INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    content_hash  TEXT NOT NULL
);
