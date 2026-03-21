---
title: Configuration
description: Server and worker configuration reference
---

Both the server and worker are configured via YAML files. Environment variables with the `STROEM__` prefix can override any config value.

## Server configuration

Create a `server-config.yaml` and point the server at it:

```bash
STROEM_CONFIG=server-config.yaml stroem-server
```

### Full example

```yaml
listen: "0.0.0.0:8080"
db:
  url: "postgres://stroem:stroem@localhost:5432/stroem"
log_storage:
  local_dir: /var/stroem/logs
  # Optional: S3 archival
  # s3:
  #   bucket: "my-stroem-logs"
  #   region: "eu-west-1"
  #   prefix: "logs/"
  #   endpoint: "http://minio:9000"  # for S3-compatible storage
workspaces:
  default:
    type: folder
    path: ./workspace
  # Git workspace example:
  # data-team:
  #   type: git
  #   url: https://github.com/org/data-workflows.git
  #   ref: main
  #   poll_interval_secs: 60
worker_token: "change-in-production"
# Optional: worker recovery and data retention settings
# recovery:
#   heartbeat_timeout_secs: 120
#   sweep_interval_secs: 60
#   unmatched_step_timeout_secs: 30
#   worker_retention_hours: 2       # Delete inactive workers older than 2h
#   log_retention_days: 30          # Delete terminal jobs and logs older than 30d
# Optional: authentication
# auth:
#   jwt_secret: "your-jwt-secret"
#   refresh_secret: "your-refresh-secret"
#   base_url: "https://stroem.company.com"  # Required for OIDC
#   providers:
#     internal:
#       provider_type: internal
#   initial_user:
#     email: admin@stroem.local
#     password: admin
# Optional: access control list (requires auth enabled)
# acl:
#   default: deny
#   rules:
#     - workspace: "*"
#       tasks: ["*"]
#       action: run
#       groups: [devops]
#     - workspace: "production"
#       tasks: ["deploy/*"]
#       action: view
#       groups: [engineering]
#       users: [contractor@ext.com]
# Optional: agent/LLM configuration
# agents:
#   providers:
#     - id: anthropic-main
#       type: anthropic
#       api_key: "${ANTHROPIC_API_KEY}"
#       model: claude-opus-4-1-20250805
#       max_tokens: 2048
#     - id: openai-gpt4
#       type: openai
#       api_key: "${OPENAI_API_KEY}"
#       model: gpt-4o
# Optional: MCP server configuration
# mcp:
#   enabled: true
```

### Server fields

| Field | Required | Description |
|-------|----------|-------------|
| `listen` | No | Bind address (default: `0.0.0.0:8080`) |
| `db.url` | Yes | PostgreSQL connection string |
| `log_storage.local_dir` | No | Directory for local log files (default: `/tmp/stroem/logs`) |
| `log_storage.s3` | No | S3 archival config (see [Log Storage](/operations/log-storage/)) |
| `workspaces` | Yes | Map of workspace definitions (see [Multi-Workspace](/guides/multi-workspace/)) |
| `worker_token` | Yes | Shared secret for worker authentication |
| `recovery` | No | Recovery sweeper settings (see [Recovery](/operations/recovery/)) |
| `auth` | No | Authentication config (see [Authentication](/operations/authentication/)) |
| `acl` | No | Access control list configuration (see [Authorization](/operations/authorization/)) |
| `agents` | No | Agent/LLM provider configuration (see [Agent Actions](/guides/agent-actions/)) |
| `mcp` | No | MCP server configuration (see [MCP Integration](/guides/mcp/)) |

### ACL configuration

Access control is optional and requires authentication to be enabled. Configure fine-grained permissions using an `acl` section:

```yaml
acl:
  default: deny    # deny | view | run (default: deny)
  rules:
    - workspace: "*"
      tasks: ["*"]
      action: run
      groups: [devops]
    - workspace: "production"
      tasks: ["deploy/*"]
      action: view
      groups: [engineering]
      users: [contractor@ext.com]
```

| Field | Description |
|-------|-------------|
| `default` | Default action when no rule matches: `deny` (invisible), `view` (read-only), `run` (full access). Defaults to `deny`. |
| `rules[].workspace` | Workspace name or `*` wildcard. Must match exactly (case-sensitive). |
| `rules[].tasks` | List of task path patterns. Paths are `"{folder}/{task}"` or `"{task}"`. Supports `*` wildcard. |
| `rules[].action` | Permission level: `run` (execute/cancel), `view` (read-only), `deny` (invisible). |
| `rules[].groups` | Group names to match (OR'd with `users`). Users must be in at least one listed group. |
| `rules[].users` | User email addresses to match (OR'd with `groups`). |

See [Authorization](/operations/authorization/) for detailed behavior, admin role, and rule evaluation order.

## Agent providers

Agent actions enable LLM integration directly in workflows. Configure provider credentials here:

```yaml
agents:
  providers:
    - id: anthropic-main
      type: anthropic
      api_key: "${ANTHROPIC_API_KEY}"
      model: claude-opus-4-1-20250805
      max_tokens: 2048
      temperature: 0.7
      max_retries: 2

    - id: openai-gpt4
      type: openai
      api_key: "${OPENAI_API_KEY}"
      model: gpt-4o
      max_tokens: 1024

    - id: ollama-local
      type: ollama
      api_endpoint: "http://localhost:11434"
      model: llama2
```

### Agent provider fields

| Field | Required | Description |
|-------|----------|-------------|
| `id` | Yes | Unique provider identifier used in workflow actions |
| `type` | Yes | Provider type: `anthropic`, `azure`, `cohere`, `deepseek`, `galadriel`, `gemini`, `groq`, `huggingface`, `hyperbolic`, `llamafile`, `mira`, `mistral`, `moonshot`, `ollama`, `openai`, `openrouter`, `perplexity`, `together`, or `xai` |
| `api_key` | Conditional | API key (not required for `ollama` and `llamafile`). Supports env var templating with `${VAR_NAME}` |
| `api_endpoint` | Conditional | Custom endpoint URL. Required for `azure`, optional for OpenAI-compatible servers |
| `model` | Yes | Model identifier (e.g., `claude-opus-4-1-20250805`, `gpt-4o`, `gemini-2.0-flash`) |
| `max_tokens` | No | Default max completion tokens (can be overridden per action) |
| `temperature` | No | Default sampling temperature (0–2) |
| `max_retries` | No | Number of retries on transient errors (default 2) |

See [Agent Actions](/guides/agent-actions/) for complete examples and provider-specific documentation.

## Worker configuration

Create a `worker-config.yaml`:

```bash
STROEM_CONFIG=worker-config.yaml stroem-worker
```

### Full example

```yaml
server_url: "http://localhost:8080"
worker_token: "change-in-production"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: /tmp/stroem-workspace

# Tags declare what this worker can run
tags:
  - script
  - docker

# Default image for script-in-container execution (runner: docker/pod)
# runner_image: "ghcr.io/fremvaerk/stroem-runner:latest"

# Optional: Docker runner
# docker: {}

# Optional: Kubernetes runner
# kubernetes:
#   namespace: stroem-jobs
#   init_image: curlimages/curl:latest
```

### Worker fields

| Field | Required | Description |
|-------|----------|-------------|
| `server_url` | Yes | Server HTTP URL |
| `worker_token` | Yes | Must match the server's `worker_token` |
| `worker_name` | No | Display name (default: hostname) |
| `max_concurrent` | No | Max concurrent step executions (default: 4) |
| `poll_interval_secs` | No | Poll frequency in seconds (default: 2) |
| `workspace_cache_dir` | No | Local cache for workspace tarballs |
| `tags` | No | Tags for step routing (default: `["script"]`) |
| `runner_image` | No | Default Docker image for `type: script` container steps |
| `docker` | No | Enable Docker runner (empty object `{}`) |
| `kubernetes` | No | Kubernetes runner config |
| `kubernetes.namespace` | No | Namespace for step pods (default: `default`) |
| `kubernetes.init_image` | No | Init container image for workspace download |

## Environment variable overrides

Both server and worker support `STROEM__` prefixed environment variables that override YAML values. Use `__` (double underscore) as the separator for nested keys:

| Env Var | Overrides YAML key |
|---------|--------------------|
| `STROEM__DB__URL` | `db.url` |
| `STROEM__WORKER_TOKEN` | `worker_token` |
| `STROEM__AUTH__JWT_SECRET` | `auth.jwt_secret` |
| `STROEM__LISTEN` | `listen` |
| `STROEM__SERVER_URL` | `server_url` |

This is particularly useful for injecting secrets without putting them in config files:

```bash
export STROEM__DB__URL="postgres://user:secret@prod-db:5432/stroem"
export STROEM__WORKER_TOKEN="production-secret-token"
STROEM_CONFIG=server-config.yaml stroem-server
```

## Database setup

Strøm requires PostgreSQL 14+. The server runs migrations automatically on startup.

```bash
# Create the database
createdb stroem

# Or via Docker
docker run -d --name postgres \
  -e POSTGRES_USER=stroem \
  -e POSTGRES_PASSWORD=stroem \
  -e POSTGRES_DB=stroem \
  -p 5432:5432 \
  postgres:16
```
