-- Content-addressed HRR codebook (sutra/327): codebook vectors are now derived
-- from a stable hash of the key at encode time, so nothing needs persisting —
-- on large C workspaces the table was 754MB of the index (90k identifiers ×
-- 8KB) and its sequential-RNG minting made encoding order-dependent.
-- Existing vectors were produced under the old per-run random basis and are
-- not comparable to newly encoded ones; clearing hrr_vectors + hrr_file_hashes
-- forces a one-time full HRR recompute on the next parse.
DROP TABLE IF EXISTS hrr_codebook;
DELETE FROM hrr_vectors;
DELETE FROM hrr_file_hashes;
