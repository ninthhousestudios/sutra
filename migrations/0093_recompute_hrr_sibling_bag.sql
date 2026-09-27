-- HRR encoding change (sutra/503): each node's children are now bundled both
-- unpositioned and position-permuted, so an inserted statement no longer
-- orthogonalises every later sibling. Existing vectors used the old encoding
-- and are not comparable to newly encoded ones; clearing hrr_vectors +
-- hrr_file_hashes forces a one-time full HRR recompute on the next parse.
DELETE FROM hrr_vectors;
DELETE FROM hrr_file_hashes;
