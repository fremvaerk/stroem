-- Re-run Prefill & Job Lineage (feature A) + reserved fields for Restart-from-step (feature B).
-- See docs/internal/2026-04-28-rerun-prefill-design.md.

ALTER TABLE job
    ADD COLUMN raw_input JSONB,
    ADD COLUMN source_job_id UUID REFERENCES job(job_id) ON DELETE SET NULL,
    ADD COLUMN restart_from_step TEXT;

-- Extend source_type CHECK to accept 'rerun' and 'restart'.
-- Previous values (after migration 031): api, trigger, webhook, task, hook, user, mcp,
-- agent_tool, event_source, retry, upload.
ALTER TABLE job DROP CONSTRAINT IF EXISTS job_source_type_check;
ALTER TABLE job ADD CONSTRAINT job_source_type_check
    CHECK (source_type IN (
        'api', 'trigger', 'webhook', 'task', 'hook',
        'user', 'mcp', 'agent_tool', 'event_source',
        'retry', 'upload',
        'rerun', 'restart'
    ));

-- Lineage lookup index for "what jobs are re-runs/restarts of X".
CREATE INDEX idx_job_source_job_id
    ON job(source_job_id)
    WHERE source_job_id IS NOT NULL;
