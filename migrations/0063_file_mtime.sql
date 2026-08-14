-- Per-file modification time (nanoseconds since epoch) captured at parse time.
-- Lets a frozen-workspace reparse skip the read+hash of unchanged files via a
-- cheap stat, instead of reading every file in the corpus (sutra/324).
-- Nullable: rows from before this migration have no baseline and fall through
-- to the content-hash check, so the change is safe for existing indexes.
ALTER TABLE files ADD COLUMN mtime_ns INTEGER;
