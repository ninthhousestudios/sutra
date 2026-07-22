-- Categories became a positive retrieval key (search tier 2), not just the
-- negative language filter they started as — tag lookup is now on a read path.
CREATE INDEX IF NOT EXISTS idx_categories_tag ON categories(tag);
