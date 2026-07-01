---
title: MCP Integration
description: Connect AI agents to Strøm via the Model Context Protocol
---

Strøm exposes a Model Context Protocol (MCP) endpoint that allows AI agents like Claude, Cursor, and other MCP-compatible tools to interact with your workflow orchestration server using a standardized interface.

## Overview

The MCP endpoint provides a set of tools for AI agents to:

- List and query available workspaces and tasks
- Retrieve detailed task definitions and execution history
- Execute tasks and monitor job status in real-time
- View logs and troubleshoot failed steps
- Cancel running or pending jobs

This enables AI agents to autonomously orchestrate complex workflows, answer questions about your infrastructure, and help debug issues.

## Enabling MCP

Add the `mcp` section to your `server-config.yaml`:

```yaml
mcp:
  enabled: true
```

The MCP endpoint is served at `http://localhost:8080/mcp` using Streamable HTTP transport (JSON response mode). When auth is configured on the server, MCP clients must send `Authorization: Bearer <token>` headers with each request.

:::note
No additional configuration is required beyond enabling MCP. The endpoint automatically exposes all available workspaces, tasks, and jobs accessible to the authenticated user.
:::

## Available Tools

The MCP server provides eight tools for interacting with Strøm:

| Tool | Description | Parameters |
|------|-------------|------------|
| `list_workspaces` | List all workspaces and their task counts | none |
| `list_tasks` | List tasks in a workspace | `workspace?` (optional; lists all if omitted) |
| `get_task` | Get detailed task definition | `workspace`, `task_name` |
| `execute_task` | Execute a task and return job ID | `workspace`, `task_name`, `input?` |
| `get_job_status` | Get current job status with step details | `job_id` |
| `get_job_logs` | Get formatted log output | `job_id`, `step?` (optional; all steps if omitted) |
| `list_jobs` | List recent jobs with optional filters | `workspace?`, `task_name?`, `status?`, `limit?` |
| `cancel_job` | Cancel a running or pending job | `job_id` |

## Authentication

When auth is configured on your Strøm server, MCP clients must authenticate using the standard HTTP `Authorization` header:

```
Authorization: Bearer <token>
```

Strøm supports two token types:

- **JWT tokens**: Long-lived (30 days) user session tokens issued at login. Supported on both API keys and user logins.
- **API keys**: Long-lived programmatic tokens with the `strm_` prefix. Ideal for MCP integrations and CI/CD pipelines. Manage API keys via the Settings page or `/api/auth/api-keys` endpoint.

MCP clients without an `Authorization` header can still connect if auth is disabled on the server.

### OAuth 2.1 (MCP spec 2025-06-18)

When auth is enabled, Strøm acts as both the OAuth 2.1 **Resource Server** for `/mcp` and the **Authorization Server** that issues tokens for it. A spec-conformant MCP client connects with no manual configuration — it discovers the auth flow on the first 401.

The flow:

1. Client calls `/mcp` without a token.
2. Strøm responds `401 Unauthorized` with `WWW-Authenticate: Bearer resource_metadata="https://.../.well-known/oauth-protected-resource"`.
3. Client fetches that document — [RFC 9728](https://datatracker.ietf.org/doc/html/rfc9728) Protected Resource Metadata — which advertises the authorization server URL.
4. Client fetches `/.well-known/oauth-authorization-server` — [RFC 8414](https://datatracker.ietf.org/doc/html/rfc8414) — to learn the endpoints, grant types, and PKCE methods. Strøm advertises:
   - `response_types_supported: ["code"]`
   - `grant_types_supported: ["authorization_code", "refresh_token"]`
   - `code_challenge_methods_supported: ["S256"]` (OAuth 2.1 forbids `plain`)
   - `token_endpoint_auth_methods_supported: ["none", "client_secret_post"]`
5. Client self-registers via `POST /oauth/register` — [RFC 7591](https://datatracker.ietf.org/doc/html/rfc7591) Dynamic Client Registration. The response includes a `client_id` (and `client_secret` if the client requested confidential auth).
6. Client opens the user's browser to `/oauth/authorize?response_type=code&client_id=...&redirect_uri=...&code_challenge=...&code_challenge_method=S256&scope=mcp&resource=https://.../mcp&state=...`.
7. Strøm validates the request, then redirects to `/consent?...` — the React consent screen. If the user isn't logged in, the SPA bounces them through `/login?next=/consent?...` first.
8. The user clicks **Allow**. The SPA `POST /api/oauth/consent` mints a single-use authorization code and returns the final redirect URL, which the browser then navigates to: `redirect_uri?code=<code>&state=<state>`.
9. Client exchanges the code at `POST /oauth/token` with the matching `code_verifier`. Strøm verifies PKCE (`BASE64URL(SHA256(code_verifier)) == stored code_challenge`), then mints:
   - An **access token** — JWT, 1 hour TTL, `aud` bound to the canonical `/mcp` URL (RFC 8707 Resource Indicators).
   - A **refresh token** — opaque, DB-backed, 30 day TTL, **rotated** on each use (OAuth 2.1 §4.13).
10. Client uses the access token as the `Bearer` value on `/mcp`. The MCP middleware enforces the `aud` claim — a token issued for a different resource is rejected, even if signed with the same secret.

#### Dynamic Client Registration (DCR)

`POST /oauth/register` is public — any client can register without an admin step. To limit abuse:

- Each registration expires after **30 days**. Long-running integrations re-register on the first request after expiry; the cost is one automatic POST.
- Maximum 5 `redirect_uris` per client.
- Only `https://...` redirects are allowed, except for `http://localhost`, `http://127.0.0.1`, and `http://[::1]` (loopback exceptions per OAuth 2.1 §4.1).
- Only the `authorization_code` + `refresh_token` grants and the `mcp` scope are permitted.

#### Audience binding

Tokens issued by `/oauth/token` carry an `aud` claim equal to the canonical resource URL (e.g. `https://stroem.example.com/mcp`). The MCP middleware verifies this on every request. A token leaked from one resource server cannot be replayed against another, even if both trust the same issuer.

Legacy `strm_*` API keys and login-flow JWTs predate the OAuth flow and have no `aud` claim — the middleware accepts them unchanged. New integrations should prefer the OAuth flow.

#### Configuration

Set `auth.base_url` in `server-config.yaml` so the discovery documents and `aud` claim resolve to the canonical public URL:

```yaml
auth:
  jwt_secret: "..."
  refresh_secret: "..."
  base_url: "https://stroem.example.com"
```

Without `base_url`, Strøm falls back to the request `Host` (respecting `X-Forwarded-Host` / `X-Forwarded-Proto`) — fine for local dev, brittle behind multiple proxy layers.

#### Claude Desktop with auto-discovery

A spec-conformant MCP client only needs the `/mcp` URL. Auth handles itself:

```json
{
  "mcpServers": {
    "stroem": {
      "url": "https://stroem.example.com/mcp",
      "transport": "streamable-http"
    }
  }
}
```

The first connection triggers the 401 → discovery → DCR → browser consent → token exchange flow described above. After the user clicks **Allow** once, Claude Desktop refreshes the access token silently for as long as the refresh token is valid (30 days).

## Connecting from Claude Desktop

To connect Claude Desktop to your Strøm server, add the MCP configuration to `claude_desktop_config.json`:

On macOS/Linux, edit `~/.claude/claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "stroem": {
      "url": "http://localhost:8080/mcp",
      "transport": "streamable-http"
    }
  }
}
```

On Windows, edit `%APPDATA%\Claude\claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "stroem": {
      "url": "http://localhost:8080/mcp",
      "transport": "streamable-http"
    }
  }
}
```

If your Strøm server requires authentication, add the token:

```json
{
  "mcpServers": {
    "stroem": {
      "url": "http://localhost:8080/mcp",
      "transport": "streamable-http",
      "headers": {
        "Authorization": "Bearer YOUR_API_KEY_OR_JWT_TOKEN"
      }
    }
  }
}
```

Restart Claude Desktop after updating the config. The Strøm tools will appear in Claude's tool palette.

## Example: Typical Agent Workflow

Here's a typical interaction sequence between an AI agent and Strøm via MCP:

### 1. Discover available workspaces

```
Call: list_workspaces
Returns:
[
  {
    "name": "production",
    "task_count": 12
  },
  {
    "name": "staging",
    "task_count": 8
  }
]
```

### 2. List tasks in a workspace

```
Call: list_tasks(workspace="production")
Returns:
[
  { "name": "deploy-api", "description": "Deploy API server" },
  { "name": "run-tests", "description": "Run test suite" },
  { "name": "backup-db", "description": "Backup production database" }
]
```

### 3. Get task details

```
Call: get_task(workspace="production", task_name="deploy-api")
Returns:
{
  "name": "deploy-api",
  "description": "Deploy API server",
  "input": {
    "version": { "type": "string", "required": true },
    "environment": { "type": "string", "default": "staging" }
  },
  "flow": [
    { "name": "pull-image", "action": "docker-pull", ... },
    { "name": "start-container", "action": "docker-run", "depends_on": ["pull-image"] }
  ]
}
```

### 4. Execute the task

```
Call: execute_task(
  workspace="production",
  task_name="deploy-api",
  input={ "version": "v1.2.3" }
)
Returns:
{
  "job_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
}
```

### 5. Monitor job status

```
Call: get_job_status(job_id="a1b2c3d4-e5f6-7890-abcd-ef1234567890")
Returns:
{
  "id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "status": "running",
  "started_at": "2025-03-12T14:30:00Z",
  "steps": [
    { "name": "pull-image", "status": "completed", "duration_secs": 45 },
    { "name": "start-container", "status": "running", "started_at": "2025-03-12T14:30:50Z" }
  ]
}
```

### 6. Retrieve logs

```
Call: get_job_logs(job_id="a1b2c3d4-e5f6-7890-abcd-ef1234567890")
Returns:
[pull-image] Pulling image us-docker.io/stroem/api:v1.2.3...
[pull-image] Pull complete (45.2s)
[start-container] Starting container stroem-api-prod...
[start-container] Container started with ID abc123def456
[start-container] Health check: passed
```

## Use Cases

### AI-Assisted Deployments

Agents can handle multi-step deployments with approval gates, automated rollbacks on health check failures, and notification of relevant teams.

### Infrastructure Troubleshooting

Agents can query job logs, identify failing steps, and suggest remediation steps based on error messages and logs.

### Automation and CI/CD

External CI/CD systems can use MCP to trigger Strøm tasks, poll job status, and integrate results into larger pipelines.

### Interactive Exploration

Agents can explore available tasks, their input requirements, and past execution patterns to answer operational questions.

## Access Control

When ACL is configured on your Strøm server, MCP tools enforce the same permission rules as the REST API:

- **List tools** (`list_workspaces`, `list_tasks`, `list_jobs`): Filter results to only show resources the user has access to. Tasks with `Deny` permission are hidden.
- **Read tools** (`get_task`, `get_job_status`, `get_job_logs`): Require `View` or `Run` permission. `Deny` returns "not found".
- **Mutation tools** (`execute_task`, `cancel_job`): Require `Run` permission. `View`-only users receive an error.

The `list_tasks` response includes a `can_execute` field indicating whether the user can execute each task (`true` for `Run`, `false` for `View`).

Jobs created via MCP are tagged with `source_type = "mcp"` and the authenticated user's email as `source_id` for audit trail.

## Best Practices

- **Use API keys for integrations**: Create dedicated API keys for MCP connections rather than sharing user credentials.
- **Set token expiry**: When creating API keys for long-running integrations, consider setting an expiry date for security.
- **Monitor MCP requests**: MCP operations are logged with the authenticated user's email as `source_id` for audit tracking.
- **Validate agent outputs**: When agents execute tasks, ensure their input validation matches your requirements (schemas are available via `get_task`).
- **Use descriptive error messages**: Include clear `error_message` fields in your task actions so agents can provide helpful troubleshooting guidance.

## Troubleshooting

### Connection Refused

Ensure your Strøm server is running and accessible at the configured URL. If behind a proxy or load balancer, verify that the `/mcp` endpoint is not blocked or rewritten.

### Authentication Errors

Verify your API key or JWT token is valid and included in the `Authorization` header. API keys must start with `strm_` prefix. Check the server logs for auth-related errors.

### Tool Not Found

Ensure MCP is enabled in `server-config.yaml` and your server has been restarted. Tools are only available after the server starts with `mcp.enabled: true`.

### Workspace or Task Not Found

Verify the workspace and task names are correct (case-sensitive). Use `list_workspaces` and `list_tasks` to confirm availability.

## See Also

- [Workflow Authoring](/docs/guides/workflow-authoring/) — Learn how to define tasks and actions
- [API Reference](/docs/api-reference/) — REST API documentation
- [Task Actions](/docs/guides/action-types/) — Available action types for tasks
