-- Add 'upload' to source_type CHECK for out-of-band state uploads
-- via POST /api/workspaces/{ws}/tasks/{task}/state and
-- POST /api/workspaces/{ws}/state. Each upload produces a synthetic
-- completed job row with source_type='upload' for audit.

ALTER TABLE job DROP CONSTRAINT IF EXISTS job_source_type_check;
ALTER TABLE job ADD CONSTRAINT job_source_type_check
    CHECK (source_type IN (
        'api', 'trigger', 'webhook', 'task', 'hook',
        'user', 'mcp', 'agent_tool', 'event_source',
        'retry', 'upload'
    ));
