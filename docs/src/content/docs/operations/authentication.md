---
title: Authentication
description: JWT authentication, OIDC SSO, and user management
---

Authentication in Strøm is **optional**. When no `auth` section is present in `server-config.yaml`, all API routes work without authentication. Adding the `auth` section enables JWT-based authentication.

## Server configuration

```yaml
auth:
  jwt_secret: "your-jwt-secret"
  refresh_secret: "your-refresh-secret"
  base_url: "https://stroem.company.com"  # Required for OIDC
  providers:
    internal:
      provider_type: internal
    # OIDC SSO provider:
    # google:
    #   provider_type: oidc
    #   display_name: "Google"
    #   issuer_url: "https://accounts.google.com"
    #   client_id: "your-client-id.apps.googleusercontent.com"
    #   client_secret: "your-client-secret"
  initial_user:
    email: admin@stroem.local
    password: admin
```

### Fields

| Field | Required | Description |
|-------|----------|-------------|
| `jwt_secret` | Yes | Secret for signing JWT access tokens (15-minute TTL) |
| `refresh_secret` | Yes | Secret for refresh token operations (30-day TTL, rotation on use) |
| `base_url` | OIDC only | Public URL of the server (for redirect URI construction) |
| `providers` | Yes | Authentication providers map |
| `initial_user` | No | Seeds an initial user on startup if one doesn't already exist |

## Internal authentication

The `internal` provider enables email/password login with argon2id password hashing.

```yaml
providers:
  internal:
    provider_type: internal
```

### Token flow

1. User logs in via `POST /api/auth/login` with email and password
2. Server returns an access token (15-minute TTL) and refresh token (30-day TTL)
3. Client includes the access token in `Authorization: Bearer <token>` headers
4. When the access token expires, client uses `POST /api/auth/refresh` with the refresh token
5. Server returns a new access/refresh token pair (old refresh token is revoked)

## OIDC SSO

Strøm supports OIDC (OpenID Connect) for single sign-on via providers like Google, GitHub, Azure AD, and Okta.

```yaml
providers:
  google:
    provider_type: oidc
    display_name: "Google"
    issuer_url: "https://accounts.google.com"
    client_id: "your-client-id.apps.googleusercontent.com"
    client_secret: "your-client-secret"
```

### Flow

1. User clicks the SSO button in the UI
2. Server redirects to the identity provider (Authorization Code + PKCE flow)
3. User authenticates with the provider
4. Provider redirects back to `/api/auth/oidc/{provider}/callback`
5. Server validates the callback, provisions the user if needed, and issues internal JWT tokens
6. User is redirected to the UI with tokens in the URL fragment

### JIT user provisioning

When a user authenticates via OIDC for the first time:
1. If an auth link for this provider + external ID exists → return that user
2. If a user with the same email exists → create an auth link and return that user
3. Otherwise → create a new user (no password) + auth link

## API keys

API keys provide long-lived tokens for programmatic and CI/CD access. They are tied to a user account and provide the same access as that user.

### Creating API keys

Use the Settings page in the UI, or the API directly:

```bash
# Create a key (requires JWT auth)
curl -X POST https://stroem.example.com/api/auth/api-keys \
  -H "Authorization: Bearer <jwt-token>" \
  -H "Content-Type: application/json" \
  -d '{"name": "CI Pipeline", "expires_in_days": 90}'
```

The response includes the raw key (shown only once):

```json
{
  "key": "strm_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "name": "CI Pipeline",
  "prefix": "strm_a1b",
  "expires_at": "2026-05-27T12:00:00Z"
}
```

### Using API keys

Use the key as a Bearer token in the `Authorization` header, exactly like a JWT:

```bash
curl https://stroem.example.com/api/jobs \
  -H "Authorization: Bearer strm_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4"
```

API keys work with the CLI too — set the token in the CLI config or pass it directly.

### Key format

Keys use the format `strm_` followed by 32 random hex characters (37 characters total). The `strm_` prefix lets the server distinguish API keys from JWT tokens. Only a SHA256 hash of the key is stored in the database.

### Managing keys

- **List**: `GET /api/auth/api-keys` (returns prefix, name, dates — never the raw key)
- **Revoke**: `DELETE /api/auth/api-keys/{prefix}`
- Keys can optionally have an expiration date
- The `last_used_at` timestamp tracks when each key was last used

## Web UI behavior

The UI automatically detects whether authentication is enabled by calling `GET /api/config`. When auth is enabled, the login page is shown. When disabled, the UI proceeds directly to the dashboard.

## Helm deployment

Use `extraSecretEnv` to inject secrets:

```yaml
server:
  config:
    auth:
      jwt_secret: "placeholder"
      refresh_secret: "placeholder"
      initial_user:
        email: admin@example.com
        password: "placeholder"
  extraSecretEnv:
    STROEM__AUTH__JWT_SECRET: "real-jwt-secret"
    STROEM__AUTH__REFRESH_SECRET: "real-refresh-secret"
    STROEM__AUTH__INITIAL_USER__PASSWORD: "real-admin-password"
```

See [Auth API](/reference/auth-api/) for the full endpoint reference.
