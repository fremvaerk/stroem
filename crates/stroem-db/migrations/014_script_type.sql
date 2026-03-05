-- Rename action_type 'shell' to 'script' and update related JSONB tag columns.

-- Phase 1: Data updates (no locks needed beyond row-level)
UPDATE job_step SET action_type = 'script' WHERE action_type = 'shell';

UPDATE job_step
  SET required_tags = (
    SELECT jsonb_agg(CASE WHEN elem::text = '"shell"' THEN '"script"'::jsonb ELSE elem END)
    FROM jsonb_array_elements(required_tags) AS elem
  )
  WHERE required_tags @> '["shell"]';

UPDATE worker
  SET tags = (
    SELECT jsonb_agg(CASE WHEN elem::text = '"shell"' THEN '"script"'::jsonb ELSE elem END)
    FROM jsonb_array_elements(tags) AS elem
  )
  WHERE tags @> '["shell"]';

-- Phase 2: DDL with lock timeout to avoid blocking production traffic.
-- If the timeout is hit the migration will fail and can be retried during a quieter window.
SET lock_timeout = '5s';

ALTER TABLE job_step DROP CONSTRAINT IF EXISTS job_step_action_type_check;
ALTER TABLE job_step ADD CONSTRAINT job_step_action_type_check
  CHECK (action_type IN ('script', 'docker', 'pod', 'task'));
ALTER TABLE job_step ALTER COLUMN action_type SET DEFAULT 'script';

RESET lock_timeout;

-- Phase 3: Refresh planner statistics for the partial GIN index on required_tags.
ANALYZE job_step;
