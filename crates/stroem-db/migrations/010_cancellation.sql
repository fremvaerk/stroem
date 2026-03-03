-- Add 'cancelled' to the allowed step status values.
-- Use a short lock timeout to avoid blocking production traffic if the table is
-- heavily contended. If the timeout is hit the migration will fail and can be
-- retried during a quieter window.
SET lock_timeout = '5s';

ALTER TABLE job_step DROP CONSTRAINT job_step_status_check;
ALTER TABLE job_step ADD CONSTRAINT job_step_status_check
    CHECK (status IN ('pending', 'ready', 'running', 'completed', 'failed', 'skipped', 'cancelled'));

RESET lock_timeout;
