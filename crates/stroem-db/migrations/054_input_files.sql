CREATE TABLE input_file (
    file_id             TEXT PRIMARY KEY CHECK (file_id ~ '^file_[0-9a-f]{32}$'),
    workspace           TEXT NOT NULL,
    task_name           TEXT NOT NULL,
    uploaded_by_user_id UUID REFERENCES "user"(user_id) ON DELETE SET NULL,
    origin              TEXT NOT NULL CHECK (origin IN ('api', 'mcp_inline', 'mcp_url')),
    original_name       TEXT NOT NULL,
    safe_name           TEXT NOT NULL,
    content_type        TEXT NOT NULL,
    size_bytes          BIGINT NOT NULL CHECK (size_bytes >= 0),
    sha256              TEXT NOT NULL,
    storage_key         TEXT NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    deleting_at         TIMESTAMPTZ
);

CREATE INDEX ix_input_file_created_at ON input_file(created_at);

CREATE TABLE job_input_file (
    job_id  UUID NOT NULL REFERENCES job(job_id) ON DELETE RESTRICT,
    field   TEXT NOT NULL,
    file_id TEXT NOT NULL REFERENCES input_file(file_id) ON DELETE RESTRICT,
    PRIMARY KEY (job_id, field)
);

CREATE INDEX ix_job_input_file_file_id ON job_input_file(file_id);

CREATE TABLE input_file_upload_token (
    token_sha256  TEXT PRIMARY KEY,
    user_id       UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
    workspace     TEXT NOT NULL,
    task_name     TEXT NOT NULL,
    original_name TEXT NOT NULL,
    expires_at    TIMESTAMPTZ NOT NULL,
    used_at       TIMESTAMPTZ
);
