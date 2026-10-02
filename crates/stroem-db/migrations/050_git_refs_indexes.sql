-- Indexes for git refs (spec 2026-10-02 § 6), under NEW names so a manual
-- CONCURRENTLY pre-run and this migration never collide.
--
-- NOTE: For zero-downtime production deployments, run these statements
-- manually before deploying this migration (049's ALTERs first — they are
-- instant and the indexes need the columns):
--   ALTER TABLE job ADD COLUMN IF NOT EXISTS git_ref TEXT;
--   ALTER TABLE job ADD COLUMN IF NOT EXISTS task_folder TEXT;
--   ALTER TABLE job_step ADD COLUMN IF NOT EXISTS action_ref TEXT;
--   ALTER TABLE job_step ADD COLUMN IF NOT EXISTS task_workspace TEXT;
--   ALTER TABLE job_step ADD COLUMN IF NOT EXISTS task_ref TEXT;
--   ALTER TABLE job_step ADD COLUMN IF NOT EXISTS task_revision TEXT;
--   ALTER TABLE job_step ADD COLUMN IF NOT EXISTS pin_releases INT NOT NULL DEFAULT 0;
--   ALTER TABLE task_state ADD COLUMN IF NOT EXISTS git_ref TEXT;
--   ALTER TABLE workspace_state ADD COLUMN IF NOT EXISTS git_ref TEXT;
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_task_state_lookup_ref
--     ON task_state (workspace, task_name, git_ref, created_at DESC, id DESC);
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_workspace_state_lookup_ref
--     ON workspace_state (workspace, git_ref, created_at DESC, id DESC);
--   CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_job_pinned_tasks
--     ON job (workspace, task_name, task_folder) WHERE git_ref IS NOT NULL;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_task_state_lookup;
--   DROP INDEX CONCURRENTLY IF EXISTS idx_workspace_state_lookup;
-- The IF NOT EXISTS / IF EXISTS clauses make 049 and this migration no-ops
-- afterward. Without the pre-run, idx_job_pinned_tasks scans all of `job`
-- under a SHARE lock (blocks job writes for the scan); the state tables are
-- empty in practice.

CREATE INDEX IF NOT EXISTS idx_task_state_lookup_ref
    ON task_state (workspace, task_name, git_ref, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_workspace_state_lookup_ref
    ON workspace_state (workspace, git_ref, created_at DESC, id DESC);
-- ACL scope for pinned jobs (§ 7.8): distinct (workspace, task, folder).
CREATE INDEX IF NOT EXISTS idx_job_pinned_tasks
    ON job (workspace, task_name, task_folder) WHERE git_ref IS NOT NULL;

DROP INDEX IF EXISTS idx_task_state_lookup;
DROP INDEX IF EXISTS idx_workspace_state_lookup;
