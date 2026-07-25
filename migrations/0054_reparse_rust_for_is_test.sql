-- Backfill for 0053. `imports.is_test` is only ever written at parse time, and
-- the pipeline skips a file whose stored content_hash still matches on disk, so
-- an index built before 0053 would keep every `#[cfg(test)]` import marked
-- production indefinitely — the sutra/290 fix silently not applying.
--
-- Clearing the hash on Rust files forces exactly one reparse of each on the next
-- pass, which repopulates is_test. Separate from 0053 because that migration may
-- already be applied, and applied migrations are content-hash pinned (sutra/293).
UPDATE files SET content_hash = '' WHERE language = 'rust';
