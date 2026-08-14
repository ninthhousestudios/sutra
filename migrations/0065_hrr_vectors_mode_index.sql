-- Index hrr_vectors by (mode, symbol_id) (sutra/328). The primary key is
-- (symbol_id, mode), so a mode-only predicate — used by the strip-only embed
-- purge, the strip-vector family load, and the embed similarity scan — could
-- not use it and fell back to a full-table scan (~1KB/row blobs). Leading with
-- mode turns those into index range scans and provides the ORDER BY symbol_id
-- ordering for free.
CREATE INDEX IF NOT EXISTS idx_hrr_vectors_mode ON hrr_vectors(mode, symbol_id);
