-- Index on job.source_type to support audit queries like
-- `GET /api/jobs?source_type=upload`. The filter added in the state-upload
-- feature makes these lookups common; without the index they seq-scan job.
-- Partial index to keep it compact — admin audit queries typically filter
-- to non-default source types.
CREATE INDEX IF NOT EXISTS idx_job_source_type
    ON job(source_type)
    WHERE source_type NOT IN ('api', 'trigger');
