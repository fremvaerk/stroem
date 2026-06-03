-- Index hygiene + size_bytes ceiling for job_artifact.
--
-- 1. Add composite index on (job_id, step_name) so delete_for_step can
--    skip the heap filter pass after the job_id index scan.
-- 2. Drop the redundant ix_job_artifact_job_id — UNIQUE (job_id, name)
--    already serves WHERE job_id = $1 prefix queries; halves write
--    amplification on artifact insert.
-- 3. Defense-in-depth ceiling on size_bytes: app enforces 100 MiB but
--    a misconfigured deployment could let a single row approach
--    i64::MAX, overflowing SUM(size_bytes) casts in the cap check.
--    1 TiB (2^40) keeps headroom above any reasonable single-file cap.

CREATE INDEX ix_job_artifact_job_step ON job_artifact(job_id, step_name);

DROP INDEX ix_job_artifact_job_id;

ALTER TABLE job_artifact
    ADD CONSTRAINT job_artifact_size_bytes_max
    CHECK (size_bytes <= 1099511627776);
