-- Templated refs (spec docs/superpowers/specs/2026-10-15-templated-refs-design.md § 8).
-- Additive. A flow step whose `ref:` is a template is created DEFERRED
-- (`action_type = 'deferred'`): it waits at `ready` until the server renders
-- `ref_template`, resolves the commit and writes the step's real columns.

ALTER TABLE job_step ADD COLUMN IF NOT EXISTS ref_template TEXT;
-- One resolver per deferred step: a short renewable lease with an owner
-- token (§ 5.3 step 0, D12). Never read by a worker.
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS resolve_lease_owner UUID;
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS resolve_lease_until TIMESTAMPTZ;

-- One commit per (job, owner workspace, ref as written / rendered) for the
-- job's whole life (§ 5.2). First writer wins: INSERT … ON CONFLICT DO NOTHING.
CREATE TABLE IF NOT EXISTS job_ref_pin (
    job_id    UUID NOT NULL REFERENCES job(job_id) ON DELETE CASCADE,
    workspace TEXT NOT NULL,
    git_ref   TEXT NOT NULL,
    commit    TEXT NOT NULL,
    PRIMARY KEY (job_id, workspace, git_ref)
);

-- Widen the action-type check (last set by 026_event_source.sql) with
-- 'deferred'. NOT VALID: no table scan under this migration's ACCESS
-- EXCLUSIVE lock. 052 validates it under SHARE UPDATE EXCLUSIVE.
ALTER TABLE job_step DROP CONSTRAINT IF EXISTS job_step_action_type_check;
ALTER TABLE job_step ADD CONSTRAINT job_step_action_type_check
    CHECK (action_type IN ('script', 'shell', 'docker', 'pod', 'task', 'agent', 'approval', 'event_source', 'deferred')) NOT VALID;
