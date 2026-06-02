ALTER TABLE conventions ADD COLUMN component_id TEXT REFERENCES components(id);
