ALTER TABLE job DROP CONSTRAINT job_status_check;
ALTER TABLE job ADD CONSTRAINT job_status_check
    CHECK (status IN ('pending', 'running', 'completed', 'failed', 'cancelled', 'skipped'));
