CREATE VIRTUAL TABLE lessons_fts USING fts5(text, content='lessons', content_rowid='rowid');

CREATE TRIGGER lessons_ai AFTER INSERT ON lessons BEGIN
    INSERT INTO lessons_fts(rowid, text) VALUES (new.rowid, new.text);
END;

CREATE TRIGGER lessons_ad AFTER DELETE ON lessons BEGIN
    INSERT INTO lessons_fts(lessons_fts, rowid, text) VALUES('delete', old.rowid, old.text);
END;

INSERT INTO lessons_fts(lessons_fts) VALUES('rebuild');
