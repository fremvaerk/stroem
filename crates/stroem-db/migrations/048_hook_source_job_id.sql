-- Hook jobs name the job that fired them in `source_job_id` (from this release
-- on), which the hook-chain depth guard walks. Before, that id lived only as
-- the UUID prefix of `source_id` (`{job_id}` or the legacy `{job_id}/{hook}`).
--
-- Backfill hook rows whose firing job still exists (`source_job_id` is an FK,
-- ON DELETE SET NULL). Only `source_type = 'hook'`: a retry's `source_id` is
-- also a job id and is not hook lineage. The CASE keeps the ::uuid cast from
-- ever seeing a `source_id` that failed the pattern.
UPDATE job h
   SET source_job_id = p.job_id
  FROM job p
 WHERE h.source_type = 'hook'
   AND h.source_job_id IS NULL
   AND p.job_id = CASE
         WHEN h.source_id ~* '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}(/|$)'
         THEN left(h.source_id, 36)::uuid
       END;
