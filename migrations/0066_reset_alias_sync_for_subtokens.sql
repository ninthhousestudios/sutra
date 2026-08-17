-- Force one re-projection of .sutra/aliases.toml (sutra/330). The projection
-- LOGIC changed — sync_aliases now emits sub-token rows for '_'-split segments
-- of namespaced [symbol] short names — but the aliases.toml CONTENT is
-- unchanged, so the hash-gated sync_aliases_if_changed would skip the rebuild
-- and the new sub-token rows would never materialise. Nulling the marker makes
-- the next startup re-project under the new logic (same mechanism 0062 used for
-- the schema change).
UPDATE alias_sync SET file_hash = NULL;
