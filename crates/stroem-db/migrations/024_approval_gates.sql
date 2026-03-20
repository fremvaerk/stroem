-- Add 'suspended' to allowed step status values
ALTER TABLE job_step DROP CONSTRAINT IF EXISTS job_step_status_check;
ALTER TABLE job_step ADD CONSTRAINT job_step_status_check
    CHECK (status IN ('pending', 'ready', 'claimed', 'running', 'completed', 'failed', 'skipped', 'cancelled', 'suspended'));

-- Add 'approval' to allowed action_type values
ALTER TABLE job_step DROP CONSTRAINT IF EXISTS job_step_action_type_check;
ALTER TABLE job_step ADD CONSTRAINT job_step_action_type_check
    CHECK (action_type IN ('script', 'shell', 'docker', 'pod', 'task', 'agent', 'approval'));

-- Timestamp when an approval step was suspended (waiting for human decision)
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS suspended_at TIMESTAMPTZ;

-- Rebuild partial GIN index to match updated claim query predicate
-- (was: NOT IN ('task', 'agent'), now: NOT IN ('task', 'agent', 'approval'))
DROP INDEX IF EXISTS idx_job_step_ready_claim;
CREATE INDEX idx_job_step_ready_claim ON job_step USING gin(required_tags)
    WHERE status = 'ready' AND action_type NOT IN ('task', 'agent', 'approval');
