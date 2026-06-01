CREATE TABLE job_artifact (
    id           UUID PRIMARY KEY,
    job_id       UUID NOT NULL REFERENCES job(job_id) ON DELETE RESTRICT,
    step_name    TEXT NOT NULL,
    name         TEXT NOT NULL,
    content_type TEXT NOT NULL,
    size_bytes   BIGINT NOT NULL CHECK (size_bytes >= 0),
    storage_key  TEXT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (job_id, name)
);

CREATE INDEX ix_job_artifact_job_id ON job_artifact(job_id);
