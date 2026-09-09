-- Per-file size in bytes captured at parse time. Paired with mtime_ns as the
-- cheap (size, mtime) fast path of the freshness drift probe (sutra/362): a file
-- whose size and mtime both match the stored baseline is clean without reading
-- its bytes; only a mismatch triggers a blake3 confirm against content_hash.
-- Nullable: rows from before this migration have no baseline and fall through
-- to the content-hash confirm, so the change is safe for existing indexes.
ALTER TABLE files ADD COLUMN size_bytes INTEGER;
