-- Track when a step became claimable (ready).
-- Needed to detect steps that sit in 'ready' state with no matching worker.
ALTER TABLE job_step ADD COLUMN ready_at TIMESTAMPTZ;

-- Backfill: steps that already passed through 'ready'.
-- Use started_at for steps that were claimed; NOW() for steps still in 'ready'.
UPDATE job_step SET ready_at = COALESCE(started_at, NOW())
WHERE status IN ('ready', 'running', 'completed', 'failed', 'skipped');

CREATE INDEX idx_job_step_ready_at ON job_step (ready_at) WHERE status = 'ready';
