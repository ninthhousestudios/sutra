-- Report-only, count-aware, content-keyed acknowledgments of individual
-- forbidden_pattern match instances (sutra/305).
--
-- Unlike constraint_waivers, these are honored ONLY on the reporting surface
-- (sutra_constraints violations, sutra_review, orient) — never by the edit-time
-- guard, which grandfathers pre-existing matches via its own introduced-only
-- content diff. Keeping acks out of the guard path structurally prevents the
-- file-waiver failure mode where a broad waiver blinds edit-time enforcement.
--
-- The content key mirrors the guard's MatchKey: (constraint_id, enclosing_symbol,
-- snippet), where snippet is the matched node's first line verbatim (node-relative,
-- so stable across line moves and re-indentation). accepted_count is what keeps
-- future byte-identical siblings governed: baseline count 1 vs disk count 2 ->
-- 1 surplus still reported.
CREATE TABLE IF NOT EXISTS constraint_instance_acks (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    constraint_id    TEXT NOT NULL,
    constraint_name  TEXT,
    file_path        TEXT NOT NULL,
    enclosing_symbol TEXT,          -- MatchKey part (nullable)
    snippet          TEXT,          -- MatchKey part (nullable)
    accepted_count   INTEGER NOT NULL DEFAULT 1,
    rationale        TEXT,          -- nullable: a bulk baseline may omit it
    acked_by         TEXT NOT NULL,
    created_at       TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_constraint_instance_acks_key
    ON constraint_instance_acks (
        constraint_id, file_path,
        COALESCE(enclosing_symbol, ''), COALESCE(snippet, '')
    );
