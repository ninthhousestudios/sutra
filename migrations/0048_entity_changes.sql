CREATE TABLE IF NOT EXISTS entity_commits (
    hash         TEXT PRIMARY KEY,
    committed_at INTEGER NOT NULL,
    author       TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS entity_changes (
    id                 INTEGER PRIMARY KEY,
    commit_hash        TEXT NOT NULL REFERENCES entity_commits(hash) ON DELETE CASCADE,
    qualified_name     TEXT NOT NULL,
    kind               TEXT NOT NULL,
    file_path          TEXT NOT NULL,
    change_type        TEXT NOT NULL,
    old_qualified_name TEXT,
    old_file_path      TEXT,
    pair_eligible      INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_entity_changes_commit ON entity_changes(commit_hash);
CREATE INDEX IF NOT EXISTS idx_entity_changes_name ON entity_changes(qualified_name);
