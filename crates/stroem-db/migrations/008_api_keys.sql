CREATE TABLE api_key (
    key_hash TEXT PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    prefix TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    expires_at TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ
);

CREATE INDEX idx_api_key_user ON api_key(user_id);
