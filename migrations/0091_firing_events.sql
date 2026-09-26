-- Split the firing log's review-event identity from its site identity
-- (sutra/491). 0089 keyed a firing on (mechanism, kind, key, file, line,
-- whole-file content fingerprint): line drift or a rebase over unrelated
-- context duplicated one logical firing, and a revert followed by an identical
-- reapply collapsed into the old one.
--
-- review_events: one row per reviewed change. patch_id hashes only the
-- removed and added lines per file (no line numbers, no context), so a rebase
-- keeps it. epoch counts how many times the change was reverted in history
-- and then reviewed again: the same patch in a new epoch is a new opportunity.
--
-- mechanism_firings: one row per flagged site within an event. A site is
-- (mechanism, kind, key, file, enclosing symbol, line text, occurrence), where
-- occurrence is the site's ordinal among identical lines in its symbol. The
-- line number is kept for display only.
--
-- 0089 rows move to events keyed 'legacy:<fingerprint>' (never equal to a
-- new patch id), one event per old fingerprint. Their occurrence is their line
-- order within that fingerprint, so duplicate lines in one symbol survive.
-- Rows that 0089 split across fingerprints by rebase stay split: the old
-- fingerprint cannot be turned back into a patch id.
CREATE TABLE IF NOT EXISTS review_events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    patch_id      TEXT    NOT NULL,
    epoch         INTEGER NOT NULL DEFAULT 0,
    surface       TEXT    NOT NULL,
    diff_spec     TEXT    NOT NULL,
    base_rev      TEXT,
    head_rev      TEXT,
    anchor_commit TEXT,
    fired_at      TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now')),
    UNIQUE (patch_id, epoch)
);

INSERT OR IGNORE INTO review_events
    (patch_id, epoch, surface, diff_spec, base_rev, head_rev, anchor_commit, fired_at)
SELECT 'legacy:' || diff_fingerprint, 0, surface, diff_spec, base_rev, head_rev,
       anchor_commit, fired_at
FROM mechanism_firings
WHERE id IN (SELECT MIN(id) FROM mechanism_firings GROUP BY diff_fingerprint);

CREATE TABLE mechanism_firings_v2 (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id     INTEGER NOT NULL REFERENCES review_events (id),
    mechanism    TEXT    NOT NULL,
    finding_kind TEXT    NOT NULL,
    finding_key  TEXT    NOT NULL,
    file_path    TEXT    NOT NULL,
    symbol       TEXT,
    snippet      TEXT,
    occurrence   INTEGER NOT NULL DEFAULT 0,
    line         INTEGER,
    fired_at     TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

INSERT OR IGNORE INTO mechanism_firings_v2
    (event_id, mechanism, finding_kind, finding_key, file_path, symbol, snippet,
     occurrence, line, fired_at)
SELECT e.id, f.mechanism, f.finding_kind, f.finding_key, f.file_path, f.symbol, f.snippet,
       ROW_NUMBER() OVER (
           PARTITION BY f.diff_fingerprint, f.mechanism, f.finding_kind, f.finding_key,
                        f.file_path, f.symbol, f.snippet
           ORDER BY f.line, f.id) - 1,
       f.line, f.fired_at
FROM mechanism_firings f
JOIN review_events e ON e.patch_id = 'legacy:' || f.diff_fingerprint AND e.epoch = 0;

DROP TABLE mechanism_firings;
ALTER TABLE mechanism_firings_v2 RENAME TO mechanism_firings;

CREATE UNIQUE INDEX IF NOT EXISTS idx_mechanism_firings_site
    ON mechanism_firings (event_id, mechanism, finding_kind, finding_key, file_path,
                          COALESCE(symbol, ''), COALESCE(snippet, ''), occurrence);

CREATE INDEX IF NOT EXISTS idx_mechanism_firings_mechanism
    ON mechanism_firings (mechanism, fired_at);
