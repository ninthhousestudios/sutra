CREATE TABLE IF NOT EXISTS commits (
    hash         TEXT PRIMARY KEY,
    committed_at INTEGER NOT NULL,
    author       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS commit_files (
    commit_hash TEXT NOT NULL REFERENCES commits(hash) ON DELETE CASCADE,
    file_id     INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    PRIMARY KEY (commit_hash, file_id)
);

CREATE INDEX IF NOT EXISTS idx_commit_files_file ON commit_files(file_id);
