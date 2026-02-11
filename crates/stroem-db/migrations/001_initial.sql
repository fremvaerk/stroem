-- Workers
CREATE TABLE worker (
    worker_id UUID PRIMARY KEY,
    name TEXT NOT NULL,
    capabilities JSONB NOT NULL DEFAULT '["shell"]',  -- e.g. ["shell", "docker", "kubernetes"]
    last_heartbeat TIMESTAMPTZ,
    registered_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'inactive'))
);

-- Jobs (the queue)
CREATE TABLE job (
    job_id UUID PRIMARY KEY,
    workspace TEXT NOT NULL DEFAULT 'default',
    task_name TEXT NOT NULL,           -- unqualified name (workspace provides the namespace)
    mode TEXT NOT NULL DEFAULT 'distributed' CHECK (mode IN ('local', 'distributed')),
    input JSONB,
    output JSONB,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'running', 'completed', 'failed', 'cancelled')),
    source_type TEXT NOT NULL CHECK (source_type IN ('trigger', 'user', 'api', 'webhook')),
    source_id TEXT,
    worker_id UUID REFERENCES worker(worker_id),
    revision TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    log_path TEXT                   -- S3 key or local path
);

CREATE INDEX idx_job_status ON job(status);
CREATE INDEX idx_job_created ON job(created_at);
CREATE INDEX idx_job_task ON job(workspace, task_name);

-- Job Steps (for distributed mode, each step is dispatchable)
CREATE TABLE job_step (
    job_id UUID NOT NULL REFERENCES job(job_id) ON DELETE CASCADE,
    step_name TEXT NOT NULL,
    action_name TEXT NOT NULL,
    action_type TEXT NOT NULL DEFAULT 'shell' CHECK (action_type IN ('shell', 'docker', 'pod')),
    action_image TEXT,                -- container image (for shell+image, docker, pod types)
    action_spec JSONB,                -- full action definition (cmd, command, env, resources, pod spec)
    input JSONB,
    output JSONB,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'ready', 'running', 'completed', 'failed', 'skipped')),
    worker_id UUID REFERENCES worker(worker_id),
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    error_message TEXT,
    PRIMARY KEY (job_id, step_name)
);

CREATE INDEX idx_job_step_status ON job_step(status);

-- Auth tables (Phase 2/3 - skipped in MVP)
-- Users
-- CREATE TABLE "user" (
--     user_id UUID PRIMARY KEY,
--     name TEXT,
--     email TEXT NOT NULL UNIQUE,
--     password_hash TEXT,
--     created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
-- );

-- Auth links (OIDC providers)
-- CREATE TABLE user_auth_link (
--     user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
--     provider_id TEXT NOT NULL,
--     external_id TEXT NOT NULL,
--     created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
--     PRIMARY KEY (user_id, provider_id)
-- );

-- Refresh tokens
-- CREATE TABLE refresh_token (
--     token_hash TEXT PRIMARY KEY,
--     user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
--     expires_at TIMESTAMPTZ NOT NULL,
--     created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
-- );

-- API keys
-- CREATE TABLE api_key (
--     key_hash TEXT PRIMARY KEY,
--     user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
--     name TEXT NOT NULL,
--     prefix TEXT NOT NULL,           -- first 8 chars for identification
--     scopes JSONB,                   -- optional: restrict to specific tasks
--     created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
--     expires_at TIMESTAMPTZ,
--     last_used_at TIMESTAMPTZ
-- );
