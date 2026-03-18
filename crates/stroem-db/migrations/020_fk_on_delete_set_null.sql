-- Fix FK constraints to allow data retention (deleting old workers and jobs).
-- All three constraints previously had no ON DELETE action (defaulting to RESTRICT),
-- which blocks DELETE on parent rows when child rows still reference them.
-- Changed to ON DELETE SET NULL so deletions propagate gracefully.

-- job.parent_job_id → job(job_id)
ALTER TABLE job DROP CONSTRAINT job_parent_job_id_fkey;
ALTER TABLE job ADD CONSTRAINT job_parent_job_id_fkey
    FOREIGN KEY (parent_job_id) REFERENCES job(job_id) ON DELETE SET NULL;

-- job.worker_id → worker(worker_id)
ALTER TABLE job DROP CONSTRAINT job_worker_id_fkey;
ALTER TABLE job ADD CONSTRAINT job_worker_id_fkey
    FOREIGN KEY (worker_id) REFERENCES worker(worker_id) ON DELETE SET NULL;

-- job_step.worker_id → worker(worker_id)
ALTER TABLE job_step DROP CONSTRAINT job_step_worker_id_fkey;
ALTER TABLE job_step ADD CONSTRAINT job_step_worker_id_fkey
    FOREIGN KEY (worker_id) REFERENCES worker(worker_id) ON DELETE SET NULL;
