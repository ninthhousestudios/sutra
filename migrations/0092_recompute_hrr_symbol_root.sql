-- HRR encoding change (sutra/471): the symbol root's node kind is no longer
-- bound into its vector, and Dart's method_signature wrapper is encoded
-- transparently. Existing vectors used the old encoding and are not
-- comparable to newly encoded ones; clearing hrr_vectors + hrr_file_hashes
-- forces a one-time full HRR recompute on the next parse.
DELETE FROM hrr_vectors;
DELETE FROM hrr_file_hashes;
