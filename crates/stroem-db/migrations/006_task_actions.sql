-- Parent job tracking for type: task sub-jobs
ALTER TABLE job ADD COLUMN parent_job_id UUID REFERENCES job(job_id);
ALTER TABLE job ADD COLUMN parent_step_name TEXT;

-- Update source_type to include 'task'
ALTER TABLE job DROP CONSTRAINT job_source_type_check;
ALTER TABLE job ADD CONSTRAINT job_source_type_check
    CHECK (source_type IN ('trigger', 'user', 'api', 'webhook', 'hook', 'task'));

-- Update action_type on job_step to include 'task'
ALTER TABLE job_step DROP CONSTRAINT job_step_action_type_check;
ALTER TABLE job_step ADD CONSTRAINT job_step_action_type_check
    CHECK (action_type IN ('shell', 'docker', 'pod', 'task'));

-- Index for finding child jobs by parent
CREATE INDEX idx_job_parent ON job(parent_job_id) WHERE parent_job_id IS NOT NULL;
