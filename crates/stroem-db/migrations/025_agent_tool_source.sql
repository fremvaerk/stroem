-- Add 'agent_tool' to source_type CHECK for child jobs created by agent tool calls.
ALTER TABLE job DROP CONSTRAINT IF EXISTS job_source_type_check;
ALTER TABLE job ADD CONSTRAINT job_source_type_check
    CHECK (source_type IN ('api', 'trigger', 'webhook', 'task', 'hook', 'user', 'mcp', 'agent_tool'));
