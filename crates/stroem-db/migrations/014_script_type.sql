-- Rename action_type 'shell' to 'script' and update related JSONB tag columns.

-- Phase 1: Drop the old CHECK constraint first so the UPDATE can set 'script'.
SET lock_timeout = '5s';
ALTER TABLE job_step DROP CONSTRAINT IF EXISTS job_step_action_type_check;
RESET lock_timeout;

-- Phase 2: Data updates
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

-- Phase 3: Add new constraint and update default.
SET lock_timeout = '5s';
ALTER TABLE job_step ADD CONSTRAINT job_step_action_type_check
  CHECK (action_type IN ('script', 'shell', 'docker', 'pod', 'task'));
ALTER TABLE job_step ALTER COLUMN action_type SET DEFAULT 'script';
RESET lock_timeout;

-- Phase 4: Refresh planner statistics for the partial GIN index on required_tags.
ANALYZE job_step;
