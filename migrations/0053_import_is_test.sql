-- Mark imports that live in test-only code (Rust `#[cfg(test)]` items).
-- Constraint evaluation excludes these edges by default: a dependency only a
-- test build can see is not an architectural dependency (sutra/290).
ALTER TABLE imports ADD COLUMN is_test INTEGER NOT NULL DEFAULT 0;
