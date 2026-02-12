-- Users
CREATE TABLE "user" (
    user_id UUID PRIMARY KEY,
    name TEXT,
    email TEXT NOT NULL UNIQUE,
    password_hash TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

-- Auth links (OIDC providers)
CREATE TABLE user_auth_link (
    user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
    provider_id TEXT NOT NULL,
    external_id TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, provider_id)
);

-- Refresh tokens
CREATE TABLE refresh_token (
    token_hash TEXT PRIMARY KEY,
    user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_refresh_token_user ON refresh_token(user_id);
