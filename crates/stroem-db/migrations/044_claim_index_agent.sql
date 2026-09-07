-- Agent steps are worker-claimed (since a8aa1c6); the retry commit ed67c76
-- re-added 'agent' to the claim query's NOT IN list, making them unclaimable.
-- The query is fixed in code; rebuild the partial GIN index so its predicate
-- is implied by the new WHERE clause (otherwise the planner cannot use it and
-- every claim poll degrades to a sequential scan of job_step).
-- (was: NOT IN ('task', 'agent', 'approval'), now: NOT IN ('task', 'approval', 'event_source'))
DROP INDEX IF EXISTS idx_job_step_ready_claim;
CREATE INDEX idx_job_step_ready_claim ON job_step USING gin(required_tags)
    WHERE status = 'ready' AND action_type NOT IN ('task', 'approval', 'event_source');
