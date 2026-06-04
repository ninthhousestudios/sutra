CREATE TABLE IF NOT EXISTS pattern_families (
    id             INTEGER PRIMARY KEY,
    member_count   INTEGER NOT NULL,
    avg_similarity REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS pattern_family_members (
    family_id  INTEGER NOT NULL REFERENCES pattern_families(id) ON DELETE CASCADE,
    symbol_id  INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
    PRIMARY KEY (family_id, symbol_id)
);
