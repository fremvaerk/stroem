-- Composite index covering the hot-path duration-stats queries that filter on
-- (workspace, task_name, status='completed') and sort by completed_at DESC.
-- The partial index predicate also constrains started_at IS NOT NULL so the
-- planner can skip rows that would be excluded by the AND completed_at >=
-- started_at guard added to the same queries.
CREATE INDEX idx_job_task_completed_at ON job (workspace, task_name, completed_at DESC)
  WHERE status = 'completed' AND started_at IS NOT NULL AND completed_at IS NOT NULL;
