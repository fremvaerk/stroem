-- Partial GIN index on ready steps to speed up the claim query.
-- The claim SQL filters on status='ready' AND action_type != 'task' with
-- a tag containment check (required_tags <@ worker_tags). This partial index
-- limits the scan to only claimable rows instead of all historical steps.
--
-- NOTE: For zero-downtime production deployments, run these statements
-- manually with CONCURRENTLY before deploying this migration:
--   DROP INDEX CONCURRENTLY IF EXISTS idx_job_step_required_tags;
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_job_step_ready_claim
--     ON job_step USING gin(required_tags) WHERE status = 'ready' AND action_type != 'task';
-- The IF NOT EXISTS / IF EXISTS clauses make this migration a no-op afterward.

-- Drop the full-table GIN index from migration 004 — the new partial index
-- below supersedes it for the only containment query (claim_ready_step).
-- Keeping both doubles GIN write overhead on every step insert/update.
DROP INDEX IF EXISTS idx_job_step_required_tags;

CREATE INDEX IF NOT EXISTS idx_job_step_ready_claim
  ON job_step USING gin(required_tags)
  WHERE status = 'ready' AND action_type != 'task';
