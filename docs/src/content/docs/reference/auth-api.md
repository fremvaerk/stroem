---
title: Auth API
description: Authentication endpoint reference
---

Auth endpoints are only available when the server is configured with an `auth` section. Without auth configuration, these endpoints return `404`.

## Login

```
POST /api/auth/login
```

Authenticates with email and password. Returns a JWT access token and a refresh token.

**Request body:**

```json
{
  "email": "admin@stroem.local",
  "password": "admin"
}
```

**Response:**

```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIs...",
  "refresh_token": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

| Status | Description |
|--------|-------------|
| `401` | Invalid email or password |
| `404` | Auth not configured |

## Refresh Token

```
POST /api/auth/refresh
```

Exchanges a refresh token for a new access/refresh token pair. The old refresh token is revoked (rotation).

**Request body:**

```json
{
  "refresh_token": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

**Response:**

```json
{
  "access_token": "eyJhbGciOiJIUzI1NiIs...",
  "refresh_token": "f1e2d3c4-b5a6-0987-dcba-0987654321fe"
}
```

| Status | Description |
|--------|-------------|
| `401` | Invalid or expired refresh token |

## Logout

```
POST /api/auth/logout
```

Revokes a refresh token.

**Request body:**

```json
{
  "refresh_token": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

**Response:**

```json
{
  "status": "ok"
}
```

## Get Current User

```
GET /api/auth/me
```

Returns the authenticated user's information. Requires a valid JWT access token.

**Headers:**

```
Authorization: Bearer <access_token>
```

**Response:**

```json
{
  "user_id": "d1e2f3a4-b5c6-7890-abcd-ef1234567890",
  "name": null,
  "email": "admin@stroem.local",
  "created_at": "2025-02-11T10:00:00Z"
}
```

| Status | Description |
|--------|-------------|
| `401` | Missing or invalid access token |

## API Keys

### Create API Key

```
POST /api/auth/api-keys
```

Creates a new API key for the authenticated user. Requires JWT authentication (not API key auth).

**Request body:**

```json
{
  "name": "CI Pipeline",
  "expires_in_days": 90
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | A descriptive name for the key |
| `expires_in_days` | No | Days until the key expires (null = never) |

**Response:**

```json
{
  "key": "strm_a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4",
  "name": "CI Pipeline",
  "prefix": "strm_a1b",
  "expires_at": "2026-05-27T12:00:00+00:00"
}
```

The `key` field is only returned at creation time. Store it securely.

| Status | Description |
|--------|-------------|
| `400` | Empty name |
| `401` | Not authenticated |

### List API Keys

```
GET /api/auth/api-keys
```

Lists the authenticated user's API keys. The raw key is never returned.

**Response:**

```json
[
  {
    "prefix": "strm_a1b",
    "name": "CI Pipeline",
    "created_at": "2026-02-26T12:00:00+00:00",
    "expires_at": "2026-05-27T12:00:00+00:00",
    "last_used_at": "2026-02-26T14:30:00+00:00"
  }
]
```

### Delete API Key

```
DELETE /api/auth/api-keys/{prefix}
```

Revokes an API key by its prefix. Only the key's owner can delete it.

| Status | Description |
|--------|-------------|
| `200` | Key revoked |
| `404` | Key not found |

## Server Config

```
GET /api/config
```

Returns server configuration for the UI. This is a **public** endpoint (no auth required).

**Response:**

```json
{
  "auth_required": true,
  "has_internal_auth": true,
  "oidc_providers": [
    { "id": "google", "display_name": "Google" }
  ]
}
```

| Field | Description |
|-------|-------------|
| `auth_required` | Whether authentication is enabled |
| `has_internal_auth` | Whether email/password login is available |
| `oidc_providers` | List of configured OIDC providers |

## OIDC Login Start

```
GET /api/auth/oidc/{provider}
```

Initiates an OIDC Authorization Code + PKCE flow. Redirects to the identity provider.

| Parameter | Description |
|-----------|-------------|
| `provider` | OIDC provider ID from config |

**Response:** `302` redirect to the identity provider's authorization endpoint.

| Status | Description |
|--------|-------------|
| `404` | Unknown OIDC provider |

## OIDC Callback

```
GET /api/auth/oidc/{provider}/callback?code=AUTH_CODE&state=CSRF_STATE
```

Handles the callback from the identity provider. Validates state, exchanges the authorization code for tokens, validates the ID token, provisions the user (JIT), and issues internal JWT tokens.

**On success:** `302` redirect to `/login/callback#access_token=AT&refresh_token=RT`

**On error:** `302` redirect to `/login/callback#error=URL_ENCODED_MSG`

### JIT user provisioning

1. If an auth link for this provider + external ID exists → return that user
2. If a user with the same email exists → create an auth link and return that user
3. Otherwise → create a new user (no password) + auth link
