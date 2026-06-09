-- Heal existing duplicate symbol rows (keep lowest id per group).
-- FTS rows first to maintain consistency, then symbols.
DELETE FROM symbols_fts WHERE symbol_id IN (
    SELECT s.id FROM symbols s
    WHERE EXISTS (
        SELECT 1 FROM symbols s2
        WHERE s2.file_id = s.file_id
          AND s2.qualified_name = s.qualified_name
          AND s2.start_line = s.start_line
          AND s2.id < s.id
    )
);

DELETE FROM symbols WHERE id IN (
    SELECT s.id FROM symbols s
    WHERE EXISTS (
        SELECT 1 FROM symbols s2
        WHERE s2.file_id = s.file_id
          AND s2.qualified_name = s.qualified_name
          AND s2.start_line = s.start_line
          AND s2.id < s.id
    )
);

-- Backstop constraint: prevent future duplicates at the DB level.
CREATE UNIQUE INDEX IF NOT EXISTS idx_symbols_unique_file_name_line
    ON symbols (file_id, qualified_name, start_line);
