CREATE TABLE IF NOT EXISTS convention_proposals (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    convention_id       TEXT    NOT NULL,
    proposed_transition TEXT    NOT NULL,
    signal_rationale    TEXT    NOT NULL,
    signal_direction    TEXT    NOT NULL,
    status              TEXT    NOT NULL DEFAULT 'pending',
    created_at          TEXT    NOT NULL DEFAULT (datetime('now')),
    resolved_at         TEXT,
    FOREIGN KEY (convention_id) REFERENCES conventions(id) ON DELETE CASCADE
);

CREATE INDEX idx_convention_proposals_status ON convention_proposals(status);
CREATE INDEX idx_convention_proposals_cid ON convention_proposals(convention_id);
