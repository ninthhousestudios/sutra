CREATE TABLE lessons (
    id              TEXT    NOT NULL PRIMARY KEY,
    text            TEXT    NOT NULL,
    created_by      TEXT    NOT NULL DEFAULT 'agent',
    created_at      TEXT    NOT NULL DEFAULT (datetime('now')),
    project_origin  TEXT,
    verified        INTEGER NOT NULL DEFAULT 0,
    confidence      INTEGER NOT NULL DEFAULT 0,
    last_surfaced   TEXT,
    last_cited      TEXT,
    archived        INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE anchors (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    lesson_id TEXT    NOT NULL REFERENCES lessons(id) ON DELETE CASCADE,
    kind      TEXT    NOT NULL,
    value     TEXT    NOT NULL
);
CREATE INDEX idx_anchors_kind_value ON anchors(kind, value);
CREATE INDEX idx_anchors_lesson_id ON anchors(lesson_id);

CREATE TABLE categories (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    lesson_id TEXT    NOT NULL REFERENCES lessons(id) ON DELETE CASCADE,
    tag       TEXT    NOT NULL,
    UNIQUE(lesson_id, tag)
);
CREATE INDEX idx_categories_lesson_id ON categories(lesson_id);

CREATE TABLE citations (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    lesson_id TEXT    NOT NULL REFERENCES lessons(id) ON DELETE CASCADE,
    task_id   TEXT    NOT NULL,
    field     TEXT,
    cited_at  TEXT    NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_citations_lesson_id ON citations(lesson_id);
