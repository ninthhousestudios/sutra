-- Drop the components.lifecycle_state column (stable/sketch "sketch mode").
-- Sketch mode's only effect was marking review deviations informational; the
-- deviation report was removed (sutra/313) and sutra_orient (sutra/312), leaving
-- the column with no reader or writer. Removed with its accessors in sutra/318.
ALTER TABLE components DROP COLUMN lifecycle_state;
