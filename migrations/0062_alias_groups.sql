-- Hierarchical alias schema (sutra/321). The flat `aliases` table (0012) only
-- modelled term -> single target. Real usage (kala-reverse's name->symbol map
-- over a decompiled binary) needs two more shapes:
--   * namespaced [symbol] terms ("positions/deg_to_rashi") resolvable by both
--     the full path AND the trailing short segment ("deg_to_rashi");
--   * array-valued [component] entries that define a MEMBERSHIP GROUP over
--     alias terms, distinct from the string-valued nickname->derived-component
--     form (which stays kind='component').
--
-- `aliases` is a pure projection of .sutra/aliases.toml (0012 already DROPs and
-- rebuilds it), so recreate with the new columns/CHECK rather than data-migrate.
DROP TABLE IF EXISTS aliases;
CREATE TABLE aliases (
    id          TEXT PRIMARY KEY,
    term        TEXT NOT NULL UNIQUE,
    short_name  TEXT,                    -- trailing segment after '/', else NULL
    target_kind TEXT NOT NULL CHECK(target_kind IN ('component','file','symbol','group')),
    target_ref  TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX idx_aliases_short_name ON aliases(short_name);

-- Membership rows for array-valued [component] groups. Keyed by term (text),
-- not FK id: the whole projection is rebuilt wholesale on each sync, so
-- referential integrity buys nothing over a simpler write path.
CREATE TABLE alias_group_member (
    group_term  TEXT NOT NULL,           -- aliases.term of the kind='group' row
    member_term TEXT NOT NULL,           -- aliases.term of a kind='symbol'/'file' row
    PRIMARY KEY (group_term, member_term)
);

-- Force one re-projection under the new schema on next startup.
UPDATE alias_sync SET file_hash = NULL;
