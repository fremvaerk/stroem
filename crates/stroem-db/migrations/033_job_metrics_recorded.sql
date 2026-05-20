-- Exactly-once guard for the `stroem_jobs_completed_total` Prometheus counter.
--
-- `metrics_recorded_at` is set to the current timestamp the first time a job
-- reaches terminal state and the counter is incremented. A CAS UPDATE
-- (`WHERE metrics_recorded_at IS NULL RETURNING job_id`) ensures only one
-- caller per job ever increments the counter, regardless of how many code
-- paths converge on terminal detection (orchestrate_after_step,
-- propagate_to_parent, handle_job_terminal, cancel_job cascade).

ALTER TABLE job ADD COLUMN metrics_recorded_at TIMESTAMPTZ;
