-- Add 'event_source' to source_type CHECK for event-source-triggered jobs.
ALTER TABLE job DROP CONSTRAINT IF EXISTS job_source_type_check;
ALTER TABLE job ADD CONSTRAINT job_source_type_check
    CHECK (source_type IN ('api', 'trigger', 'webhook', 'task', 'hook', 'user', 'mcp', 'agent_tool', 'event_source'));
