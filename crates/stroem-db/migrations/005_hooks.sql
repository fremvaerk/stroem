-- Add 'hook' to the allowed source_type values for jobs
ALTER TABLE job DROP CONSTRAINT job_source_type_check;
ALTER TABLE job ADD CONSTRAINT job_source_type_check
    CHECK (source_type IN ('trigger', 'user', 'api', 'webhook', 'hook'));
