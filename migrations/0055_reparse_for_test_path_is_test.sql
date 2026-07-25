-- Backfill for path-based test scope (sutra/292). `imports.is_test` is only
-- ever written at parse time, and the pipeline skips a file whose stored
-- content_hash still matches on disk, so imports in Rust `tests/`/`benches/`
-- targets and in every Dart test file would stay marked production until the
-- file happened to change.
--
-- Clearing the hash forces exactly one reparse of each on the next pass. Dart
-- is included because it had no test-scope detection at all before this change.
UPDATE files SET content_hash = '' WHERE language IN ('rust', 'dart');
