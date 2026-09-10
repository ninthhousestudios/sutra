-- Add `signature` to the symbols_fts index so a symbol findable only by its
-- signature — a query term that appears solely in a parameter name or type —
-- surfaces in explore's lexical stage (sutra/371).
--
-- FTS5 virtual tables cannot `ALTER TABLE ... ADD COLUMN`, so the only way to
-- widen the schema is to drop and recreate the table. Repopulation reads
-- signature straight from symbols.signature, which is already stored durably
-- per symbol (find --detail returns it), so no reparse is needed: an
-- INSERT..SELECT rebuild is complete and atomic. This differs from the
-- 0054/0055 test-scope backfills, which forced a reparse only because
-- imports.is_test had no other source.
--
-- ephemeral_only=true in the runner: symbols_fts is an Ephemeral table that
-- reindex drops and 0001 recreates with the old 4-column shape, so this must
-- replay to re-add the column. On a fresh index / reindex the symbols table is
-- empty at migration time, so the SELECT inserts nothing and the parse pass
-- repopulates via the insert sites (which now write signature too).
DROP TABLE IF EXISTS symbols_fts;

CREATE VIRTUAL TABLE symbols_fts USING fts5(
    symbol_id UNINDEXED,
    short_name,
    qualified_name,
    docstring,
    signature
);

INSERT INTO symbols_fts (symbol_id, short_name, qualified_name, docstring, signature)
SELECT id, short_name, qualified_name, docstring, signature FROM symbols;
