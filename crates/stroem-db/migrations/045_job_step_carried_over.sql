-- Restart From Step (spec 2026-09-07 §5): marks rows copied from a source job
-- (status/output carried over, never executed in this job). The source job
-- itself is job.source_job_id.
ALTER TABLE job_step ADD COLUMN carried_over BOOLEAN NOT NULL DEFAULT FALSE;
