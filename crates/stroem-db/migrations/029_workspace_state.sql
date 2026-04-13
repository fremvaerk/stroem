CREATE TABLE workspace_state (
    id              UUID PRIMARY KEY,
    workspace       TEXT NOT NULL,
    written_by_task TEXT NOT NULL,
    job_id          UUID REFERENCES job(job_id) ON DELETE SET NULL,
    storage_key     TEXT NOT NULL,
    size_bytes      BIGINT NOT NULL DEFAULT 0,
    has_json        BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

COMMENT ON COLUMN workspace_state.written_by_task IS 'Provenance: name of the task that produced this snapshot (not a scoping column)';

CREATE INDEX idx_workspace_state_lookup
    ON workspace_state(workspace, created_at DESC, id DESC);

CREATE INDEX idx_workspace_state_job
    ON workspace_state(job_id);
