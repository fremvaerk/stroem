-- Cross-workspace action references: a flow step may reference an action that
-- lives in a DIFFERENT workspace (`owner_ws.action`). The step then executes
-- with the owner workspace's tarball + secrets + connections. These two columns
-- record the owner and a pinned revision. NULL ⇒ the step's action belongs to
-- the job's own workspace (existing behaviour; no backfill required).
ALTER TABLE job_step ADD COLUMN action_workspace TEXT;
ALTER TABLE job_step ADD COLUMN action_revision TEXT;
