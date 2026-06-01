ALTER TABLE components ADD COLUMN dissolved_at TEXT;
ALTER TABLE components ADD COLUMN prior_paths TEXT;

-- Backfill prior_paths from current membership (if any)
UPDATE components SET prior_paths = (
    SELECT json_group_array(f.path)
    FROM component_membership cm
    JOIN files f ON f.id = cm.file_id
    WHERE cm.component_id = components.id
);
