-- Indexes behind the trigger last-run lookup (spec 2026-10-20 § 7.2): each
-- probe must cost the same however many skipped rows, emitted event-source
-- jobs or old jobs exist. `idx_job_retry_of` (027) already serves the retry
-- chain probe.
--
-- NOTE: For zero-downtime production deployments, run these statements
-- manually before deploying this migration, one at a time, in autocommit:
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_job_trigger_runs
--     ON job (source_type, source_id, created_at DESC)
--     WHERE (source_type IN ('trigger', 'webhook') AND status <> 'skipped')
--        OR (source_type = 'event_source' AND input ? '_fingerprint');
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_job_trigger_skipped
--     ON job (source_id, created_at DESC)
--     WHERE source_type = 'trigger' AND status = 'skipped';
-- The IF NOT EXISTS clauses make this migration a no-op afterward. Without
-- the pre-run, each index scans all of `job` under a SHARE lock (blocks
-- writes to `job` for the scan).

CREATE INDEX IF NOT EXISTS idx_job_trigger_runs
    ON job (source_type, source_id, created_at DESC)
    WHERE (source_type IN ('trigger', 'webhook') AND status <> 'skipped')
       OR (source_type = 'event_source' AND input ? '_fingerprint');
CREATE INDEX IF NOT EXISTS idx_job_trigger_skipped
    ON job (source_id, created_at DESC)
    WHERE source_type = 'trigger' AND status = 'skipped';
