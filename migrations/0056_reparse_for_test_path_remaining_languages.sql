-- Backfill for path-based test scope in Python, C and JS/TS (sutra/295), the
-- languages sutra/292 left at the default `is_test_path == false`. Same
-- mechanism as 0054 and 0055: `imports.is_test` is only ever written at parse
-- time and the pipeline skips a file whose stored content_hash still matches
-- disk, so clearing the hash forces exactly one reparse that repopulates it.
UPDATE files SET content_hash = ''
WHERE language IN ('python', 'c', 'javascript', 'typescript');
