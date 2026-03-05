-- CHECK constraints for timeout columns
ALTER TABLE job ADD CONSTRAINT job_timeout_positive CHECK (timeout_secs > 0);
ALTER TABLE job_step ADD CONSTRAINT job_step_timeout_positive CHECK (timeout_secs > 0);

-- Indexes for timeout sweep queries (recovery.rs)
CREATE INDEX idx_job_active_timeout ON job (created_at)
  WHERE status IN ('pending', 'running') AND timeout_secs IS NOT NULL;

CREATE INDEX idx_job_step_running_timeout ON job_step (started_at)
  WHERE status = 'running' AND timeout_secs IS NOT NULL;

-- Index for concurrency policy queries (scheduler.rs)
CREATE INDEX idx_job_source_active ON job (source_type, source_id)
  WHERE status IN ('pending', 'running');
