PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;

-- ---------------------------------------------------------------------------
-- files
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS files (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    path          TEXT    NOT NULL UNIQUE,
    language      TEXT    NOT NULL,
    content_hash  TEXT    NOT NULL,
    line_count    INTEGER NOT NULL,
    parsed_ok     BOOLEAN NOT NULL,
    last_parsed   TIMESTAMP NOT NULL,
    fan_in_files  INTEGER NOT NULL DEFAULT 0,
    blast_radius  INTEGER NOT NULL DEFAULT 0,
    pagerank      REAL
);

CREATE INDEX IF NOT EXISTS idx_files_fan_in ON files (fan_in_files);
CREATE INDEX IF NOT EXISTS idx_files_blast  ON files (blast_radius);

-- ---------------------------------------------------------------------------
-- symbols
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS symbols (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id           INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    qualified_name    TEXT    NOT NULL,
    short_name        TEXT    NOT NULL,
    kind              TEXT    NOT NULL,
    signature         TEXT,
    signature_hash    TEXT,
    visibility        TEXT,
    start_line        INTEGER NOT NULL,
    start_col         INTEGER NOT NULL,
    end_line          INTEGER NOT NULL,
    end_col           INTEGER NOT NULL,
    parent_symbol_id  INTEGER REFERENCES symbols(id),
    docstring         TEXT,
    pagerank          REAL,
    cyclomatic        INTEGER,
    cognitive         INTEGER
);

CREATE INDEX IF NOT EXISTS idx_symbols_short_name     ON symbols (short_name);
CREATE INDEX IF NOT EXISTS idx_symbols_qualified_name ON symbols (qualified_name);
CREATE INDEX IF NOT EXISTS idx_symbols_file           ON symbols (file_id);

-- ---------------------------------------------------------------------------
-- symbols_fts  (FTS5 — no triggers; synced manually from Rust)
-- ---------------------------------------------------------------------------

CREATE VIRTUAL TABLE IF NOT EXISTS symbols_fts USING fts5(
    symbol_id UNINDEXED,
    short_name,
    qualified_name,
    docstring
);

-- ---------------------------------------------------------------------------
-- refs
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS refs (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id           INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    target_symbol_id  INTEGER REFERENCES symbols(id) ON DELETE CASCADE,
    unresolved_name   TEXT,
    line              INTEGER NOT NULL,
    col               INTEGER NOT NULL,
    context_kind      TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_refs_target     ON refs (target_symbol_id);
CREATE INDEX IF NOT EXISTS idx_refs_file       ON refs (file_id);
CREATE INDEX IF NOT EXISTS idx_refs_unresolved ON refs (unresolved_name)
    WHERE target_symbol_id IS NULL;

-- ---------------------------------------------------------------------------
-- imports
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS imports (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    file_id           INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
    imported_path     TEXT    NOT NULL,
    resolved_file_id  INTEGER REFERENCES files(id),
    line              INTEGER NOT NULL
);

-- ---------------------------------------------------------------------------
-- snapshots
-- ---------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS snapshots (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp          TIMESTAMP NOT NULL,
    files_parsed       INTEGER   NOT NULL,
    symbols_extracted  INTEGER   NOT NULL,
    refs_extracted     INTEGER   NOT NULL,
    parse_errors       INTEGER   NOT NULL,
    duration_ms        INTEGER   NOT NULL
);
