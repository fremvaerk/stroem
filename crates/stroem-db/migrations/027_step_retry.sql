-- Step retry columns: in-place retry of individual steps on failure.
-- retry_attempt: current attempt number (0 = initial, 1 = first retry, etc.)
ALTER TABLE job_step ADD COLUMN retry_attempt INTEGER NOT NULL DEFAULT 0;
-- max_retries: resolved max retry attempts from config (NULL = no retry)
ALTER TABLE job_step ADD COLUMN max_retries INTEGER;
-- retry_backoff_secs: base delay in seconds between retries
ALTER TABLE job_step ADD COLUMN retry_backoff_secs INTEGER;
-- retry_strategy: 'fixed' or 'exponential'
ALTER TABLE job_step ADD COLUMN retry_strategy TEXT;
-- retry_jitter: whether to add random jitter to backoff delay
ALTER TABLE job_step ADD COLUMN retry_jitter BOOLEAN NOT NULL DEFAULT false;
-- retry_history: JSONB array of previous attempt details
-- Each element: {"attempt": N, "error": "...", "started_at": "...", "failed_at": "..."}
ALTER TABLE job_step ADD COLUMN retry_history JSONB NOT NULL DEFAULT '[]'::jsonb;
-- retry_at: earliest time this step can be claimed (for backoff delays). NULL = immediately.
ALTER TABLE job_step ADD COLUMN retry_at TIMESTAMPTZ;

-- Job retry columns: task-level retry creates new jobs on failure.
-- retry_of_job_id: points to the original (root) job this is a retry of
ALTER TABLE job ADD COLUMN retry_of_job_id UUID REFERENCES job(job_id) ON DELETE SET NULL;
CREATE INDEX idx_job_retry_of ON job(retry_of_job_id) WHERE retry_of_job_id IS NOT NULL;
-- retry_job_id: points to the retry job created when this job failed
ALTER TABLE job ADD COLUMN retry_job_id UUID;
-- retry_attempt: which attempt this job is (0 = original, 1 = first retry)
ALTER TABLE job ADD COLUMN retry_attempt INTEGER NOT NULL DEFAULT 0;
-- max_retries: from task retry config (NULL = no task retry)
ALTER TABLE job ADD COLUMN max_retries INTEGER;

-- Add 'retry' to source_type CHECK constraint for task-level retry jobs.
ALTER TABLE job DROP CONSTRAINT IF EXISTS job_source_type_check;
ALTER TABLE job ADD CONSTRAINT job_source_type_check
    CHECK (source_type IN ('api', 'trigger', 'webhook', 'task', 'hook', 'user', 'mcp', 'agent_tool', 'event_source', 'retry'));
