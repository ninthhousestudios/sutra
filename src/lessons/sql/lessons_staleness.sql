ALTER TABLE lessons ADD COLUMN verified_at TEXT;

CREATE TABLE anchor_verification (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    lesson_id    TEXT    NOT NULL REFERENCES lessons(id) ON DELETE CASCADE,
    anchor_id    INTEGER NOT NULL REFERENCES anchors(id) ON DELETE CASCADE,
    content_hash TEXT    NOT NULL,
    verified_at  TEXT    NOT NULL,
    UNIQUE(anchor_id)
);
CREATE INDEX idx_anchor_verification_lesson ON anchor_verification(lesson_id);
