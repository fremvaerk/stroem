CREATE TABLE task_state (
    id          UUID PRIMARY KEY,
    workspace   TEXT NOT NULL,
    task_name   TEXT NOT NULL,
    job_id      UUID REFERENCES job(job_id) ON DELETE SET NULL,
    storage_key TEXT NOT NULL,
    size_bytes  BIGINT NOT NULL DEFAULT 0,
    has_json    BOOLEAN NOT NULL DEFAULT FALSE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Primary lookup: latest snapshot for a workspace+task
CREATE INDEX idx_task_state_lookup
    ON task_state(workspace, task_name, created_at DESC);

-- Cleanup: find snapshots by job
CREATE INDEX idx_task_state_job
    ON task_state(job_id);
