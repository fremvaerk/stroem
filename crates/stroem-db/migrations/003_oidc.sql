-- Unique index for OIDC provider lookups (find user by provider + external ID)
CREATE UNIQUE INDEX IF NOT EXISTS idx_user_auth_link_provider_external
    ON user_auth_link(provider_id, external_id);
