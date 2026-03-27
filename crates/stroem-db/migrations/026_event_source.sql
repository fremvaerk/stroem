-- Add 'event_source' to source_type CHECK for event-source-triggered jobs.
ALTER TABLE job DROP CONSTRAINT IF EXISTS job_source_type_check;
ALTER TABLE job ADD CONSTRAINT job_source_type_check
    CHECK (source_type IN ('api', 'trigger', 'webhook', 'task', 'hook', 'user', 'mcp', 'agent_tool', 'event_source'));

-- Add 'event_source' to job_step.action_type CHECK so event-source steps can be inserted.
-- Event-source steps use action_type = 'event_source' and are claimed only by workers
-- that carry the 'event_source' tag.
ALTER TABLE job_step DROP CONSTRAINT IF EXISTS job_step_action_type_check;
ALTER TABLE job_step ADD CONSTRAINT job_step_action_type_check
    CHECK (action_type IN ('script', 'shell', 'docker', 'pod', 'task', 'agent', 'approval', 'event_source'));

-- Rebuild the partial GIN index used for step claiming to exclude event_source steps.
-- Event-source steps are claimed through a separate endpoint (not the regular claim path).
DROP INDEX IF EXISTS idx_job_step_ready_claim;
CREATE INDEX idx_job_step_ready_claim ON job_step USING gin(required_tags)
    WHERE status = 'ready' AND action_type NOT IN ('task', 'agent', 'approval', 'event_source');
