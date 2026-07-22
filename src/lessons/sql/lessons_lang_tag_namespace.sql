-- Language claims move into an explicit `lang:` namespace so the Phase 3
-- workspace filter can recognise them syntactically instead of matching against
-- a dictionary. A dictionary miss on the read path was a silent leak: an
-- unrecognised tag read as "makes no language claim" and the lesson surfaced in
-- every workspace. Recognition now happens once, at write time
-- (`normalize_category`), where a miss merely mislabels one tag.
--
-- The name list is frozen at the adapter set as of this migration on purpose:
-- a migration is a point-in-time data rewrite, and rows can only carry a
-- language sutra could already name. Later adapters are handled on write.
--
-- OR REPLACE, not OR IGNORE: a lesson carrying both "Rust" and "rust" would
-- collide on UNIQUE(lesson_id, tag). OR IGNORE would leave the loser behind
-- unprefixed — exactly the row this migration exists to remove.
UPDATE OR REPLACE categories
SET tag = 'lang:' || lower(tag)
WHERE lower(tag) IN (
    'rust', 'dart', 'c', 'python', 'javascript', 'typescript'
);

-- Common shorthands, folded onto the same canonical names the writer now uses.
UPDATE OR REPLACE categories SET tag = 'lang:rust'       WHERE lower(tag) = 'rs';
UPDATE OR REPLACE categories SET tag = 'lang:typescript' WHERE lower(tag) IN ('ts', 'tsx');
UPDATE OR REPLACE categories SET tag = 'lang:javascript' WHERE lower(tag) IN ('js', 'jsx');
UPDATE OR REPLACE categories SET tag = 'lang:python'     WHERE lower(tag) IN ('py', 'python3');
UPDATE OR REPLACE categories SET tag = 'lang:go'         WHERE lower(tag) IN ('go', 'golang');
