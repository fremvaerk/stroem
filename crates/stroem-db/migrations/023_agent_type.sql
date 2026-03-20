-- Add 'agent' to allowed action_type values
ALTER TABLE job_step DROP CONSTRAINT IF EXISTS job_step_action_type_check;
ALTER TABLE job_step ADD CONSTRAINT job_step_action_type_check
    CHECK (action_type IN ('script', 'shell', 'docker', 'pod', 'task', 'agent'));

-- Agent conversation state for multi-turn agent loops (Phase B)
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS agent_state JSONB;

-- Rebuild partial GIN index to match updated claim query predicate
-- (was: action_type != 'task', now: NOT IN ('task', 'agent'))
DROP INDEX IF EXISTS idx_job_step_ready_claim;
CREATE INDEX idx_job_step_ready_claim ON job_step USING gin(required_tags)
    WHERE status = 'ready' AND action_type NOT IN ('task', 'agent');
