-- Per-file health-findings coverage stamp (sutra/408).
--
-- "Missing analysis is never zero debt" only holds if scoring can tell a file
-- whose producers ran and found nothing (genuinely clean) from a file whose
-- producers never ran for its current content (unknown → worst-cased). Absence
-- of a finding cannot distinguish the two. This table records, per file, the
-- content_hash for which compute_all_health_findings last produced findings.
--
-- score_workspace compares each file's current content_hash to its coverage
-- stamp: a mismatch (incrementally reparsed file whose findings were never
-- recomputed) or a missing row (newly added file) means the file's file-scored
-- biomarkers are treated as missing and worst-cased, not floored at BASE_SCORE.
--
-- Ephemeral: coverage is meaningless without the health_findings it pairs with,
-- and both are rebuilt from scratch by a full parse. FK cascade drops the row
-- when its file is deleted (same as health_findings).
CREATE TABLE IF NOT EXISTS health_coverage (
    file_id      INTEGER PRIMARY KEY REFERENCES files(id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL
);
