-- OAuth 2.1 schema for MCP authentication.
--
-- See the MCP authorization spec (rev 2025-06-18) and RFCs 6749/7591/7636/8414/8707/9728.
--
-- Three tables:
--
--   oauth_client                — OAuth clients (DCR-registered or admin-created).
--   oauth_authorization_code    — short-lived (5min) PKCE codes from /oauth/authorize.
--   oauth_refresh_token         — long-lived refresh tokens issued by /oauth/token.

CREATE TABLE oauth_client (
    client_id TEXT PRIMARY KEY,
    -- NULL = public client (PKCE-only, e.g. Claude Desktop / Cursor).
    -- Non-NULL = SHA-256 hex of the assigned client_secret (confidential client).
    client_secret_hash TEXT,
    client_name TEXT NOT NULL,
    -- Allowed redirect URIs as a JSONB string array. Exact-match check at /authorize.
    redirect_uris JSONB NOT NULL,
    -- Allowed grant types — typically ["authorization_code","refresh_token"].
    grant_types JSONB NOT NULL DEFAULT '["authorization_code","refresh_token"]'::jsonb,
    -- Scopes the client may request. Today we only advertise "mcp".
    scope TEXT NOT NULL DEFAULT 'mcp',
    -- True for clients minted via POST /oauth/register (RFC 7591).
    -- Lets operators distinguish self-registered MCP clients from admin-managed ones.
    is_dynamic BOOLEAN NOT NULL DEFAULT FALSE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    -- Optional sliding-window expiry for DCR clients.
    expires_at TIMESTAMPTZ
);

CREATE INDEX idx_oauth_client_expires_at ON oauth_client(expires_at)
    WHERE expires_at IS NOT NULL;

-- One-time authorization codes. Hashed on insert; we never store the raw code.
CREATE TABLE oauth_authorization_code (
    code_hash TEXT PRIMARY KEY,
    client_id TEXT NOT NULL REFERENCES oauth_client(client_id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
    redirect_uri TEXT NOT NULL,
    -- PKCE (RFC 7636) — code_challenge captured at /authorize, verified at /token.
    code_challenge TEXT NOT NULL,
    code_challenge_method TEXT NOT NULL DEFAULT 'S256',
    scope TEXT NOT NULL,
    -- Resource indicator (RFC 8707) — binds the resulting access token to a
    -- specific resource (the canonical /mcp URL). Without this, a token issued
    -- for /mcp could be replayed against any future audience-bound endpoint.
    resource TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    -- Stamped at first /oauth/token exchange so a second attempt is rejected
    -- (single-use codes, OAuth 2.1 §4.1.2).
    used_at TIMESTAMPTZ
);

CREATE INDEX idx_oauth_authz_code_expires_at ON oauth_authorization_code(expires_at);
CREATE INDEX idx_oauth_authz_code_user ON oauth_authorization_code(user_id);

-- Refresh tokens issued by /oauth/token. Separate from the existing
-- `refresh_token` table so the login flow and the OAuth flow can evolve
-- independently; OAuth refresh tokens are client-bound and audience-bound.
CREATE TABLE oauth_refresh_token (
    token_hash TEXT PRIMARY KEY,
    client_id TEXT NOT NULL REFERENCES oauth_client(client_id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES "user"(user_id) ON DELETE CASCADE,
    scope TEXT NOT NULL,
    resource TEXT NOT NULL,
    expires_at TIMESTAMPTZ NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    revoked_at TIMESTAMPTZ
);

CREATE INDEX idx_oauth_refresh_token_user ON oauth_refresh_token(user_id);
CREATE INDEX idx_oauth_refresh_token_expires_at ON oauth_refresh_token(expires_at);
