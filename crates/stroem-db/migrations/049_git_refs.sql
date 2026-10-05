-- Git refs on action, task and trigger references (spec 2026-10-02 § 6).
-- Columns only: adding nullable / constant-default columns is a metadata-only
-- change in Postgres 11+, so this migration is instant. Indexes are in 050.
--
-- `git_ref` is the spec's `ref` (named so it is never confused with the Rust
-- keyword or an SQL keyword). A job with `git_ref IS NOT NULL` is a pinned job:
-- `revision` holds its commit for the job's whole life.
ALTER TABLE job ADD COLUMN IF NOT EXISTS git_ref TEXT;
ALTER TABLE job ADD COLUMN IF NOT EXISTS task_folder TEXT;

-- A step whose action was resolved through `ref:`; `action_workspace` /
-- `action_revision` describe that pin.
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS action_ref TEXT;
-- The TASK owner of a `type: task` step and its pin, stamped at parent creation.
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS task_workspace TEXT;
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS task_ref TEXT;
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS task_revision TEXT;
-- How often a claim was released because its pin was unavailable (§ 7.2).
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS pin_releases INT NOT NULL DEFAULT 0;

-- State partitions: NULL = unpinned (every existing row).
ALTER TABLE task_state ADD COLUMN IF NOT EXISTS git_ref TEXT;
ALTER TABLE workspace_state ADD COLUMN IF NOT EXISTS git_ref TEXT;
