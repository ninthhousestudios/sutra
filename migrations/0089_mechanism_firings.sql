-- Shared firing log for write-side mechanisms (sutra/467; design rule 9 in
-- docs/sutra-purpose.md). One row per site a mechanism flagged: the sibling
-- pattern check (467) is the first writer; swallow (486), dup-exists (469) and
-- orphans (483) reuse it. sutra/485 joins these rows to later code to measure
-- each mechanism's acted-on rate.
--
-- Durable: firings are history, not recomputable from code. A reindex must not
-- drop them.
--
-- diff_fingerprint identifies the reviewed change (both sides' content), so
-- reviewing the same diff twice does not count as a second firing.
CREATE TABLE IF NOT EXISTS mechanism_firings (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    mechanism        TEXT    NOT NULL,
    finding_kind     TEXT    NOT NULL,
    finding_key      TEXT    NOT NULL,
    file_path        TEXT    NOT NULL,
    line             INTEGER,
    symbol           TEXT,
    snippet          TEXT,
    surface          TEXT    NOT NULL,
    diff_spec        TEXT    NOT NULL,
    base_rev         TEXT,
    head_rev         TEXT,
    anchor_commit    TEXT,
    diff_fingerprint TEXT    NOT NULL,
    fired_at         TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_mechanism_firings_unique
    ON mechanism_firings (mechanism, finding_kind, finding_key, file_path,
                          COALESCE(line, -1), diff_fingerprint);

CREATE INDEX IF NOT EXISTS idx_mechanism_firings_mechanism
    ON mechanism_firings (mechanism, fired_at);
