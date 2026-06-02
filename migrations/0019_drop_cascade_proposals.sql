-- Remove ON DELETE CASCADE from convention_proposals.
-- Proposals are durable state; they must survive ephemeral convention pruning
-- so dismissed proposals can block re-triggering.
CREATE TABLE convention_proposals_new (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    convention_id       TEXT    NOT NULL,
    proposed_transition TEXT    NOT NULL,
    signal_rationale    TEXT    NOT NULL,
    signal_direction    TEXT    NOT NULL,
    status              TEXT    NOT NULL DEFAULT 'pending',
    created_at          TEXT    NOT NULL DEFAULT (datetime('now')),
    resolved_at         TEXT
);
INSERT INTO convention_proposals_new SELECT * FROM convention_proposals;
DROP TABLE convention_proposals;
ALTER TABLE convention_proposals_new RENAME TO convention_proposals;
CREATE INDEX idx_convention_proposals_status ON convention_proposals(status);
CREATE INDEX idx_convention_proposals_cid ON convention_proposals(convention_id);
