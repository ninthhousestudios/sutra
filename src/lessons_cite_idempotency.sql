CREATE UNIQUE INDEX IF NOT EXISTS idx_citations_idempotent
    ON citations(lesson_id, task_id, field);
