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
  #   triggers: false   # load it, but never fire its schedules/webhooks/event sources here
worker_token: "change-in-production"
# Optional: fleet-wide timeout defaults applied when a task or step does not
# specify its own `timeout`. Capped at 24h (step) and 7d (job). Omit to leave
# tasks without explicit timeouts unbounded (existing behaviour).
# default_step_timeout: 30m
# default_job_timeout: 4h
# Optional: worker recovery settings
# recovery:
#   heartbeat_timeout_secs: 120
#   sweep_interval_secs: 60
#   unmatched_step_timeout_secs: 30
# Optional: data retention settings
# retention:
#   worker_hours: 2                 # Delete inactive workers older than 2h
#   job_days: 30                    # Delete terminal jobs and logs 30d after they finished
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
| `workspaces.<name>.triggers` | No | `false` loads the workspace but never fires its triggers on this server (default: `true`; see [Disabling triggers per server](/guides/multi-workspace/#disabling-triggers-per-server)) |
| `workspace_reload` | No | Tunes workspace refresh timing and backoff (see [`workspace_reload`](#workspace_reload) below) |
| `pin_store` | No | Where and how many pinned commits this replica keeps for [git refs](/guides/git-refs/) (see [`pin_store`](#pin_store) below) |
| `worker_token` | Yes | Shared secret for worker authentication |
| `recovery` | No | Recovery sweeper settings (see [Recovery](/operations/recovery/)) |
| `retention` | No | Data retention settings (see [Retention](/operations/retention/)) |
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

### `workspace_reload`

Tunes how the server refreshes workspaces. All fields are optional.

| Field | Default | Meaning |
|---|---|---|
| `peek_failure_threshold` | `5` | Consecutive failed change checks before a forced reload |
| `peek_timeout_secs` | `30` | Budget for one change check (`ls-remote` / folder hash). At most `86400` (24 h) |
| `load_timeout_secs` | `300` | Budget for one load — see the note below. At most `86400` (24 h) |
| `max_backoff_secs` | `900` | Cap of the retry backoff for a workspace whose reload failed. At most `86400` (24 h) |
| `git_connect_timeout_ms` | `10000` | libgit2 TCP connect timeout (process-wide) |
| `git_read_timeout_ms` | `60000` | libgit2 per-read socket timeout (process-wide) |

`load_timeout_secs` **aborts** the load when it expires — for the initial load at startup, for watcher reloads and for API/trigger/peer reloads alike. The abort is cooperative: it is checked during the git transfer, checkout planning, the YAML scan, and `sops`/`vals` (which are killed). A load that ends on the deadline fails like any other failed load. This means a repository whose **first clone** takes longer than `load_timeout_secs` can never finish loading — raise the value for very large repositories. The few steps that cannot be interrupted (DNS, a single blocked socket read, the checkout write phase) keep running past the deadline and are reported by `stroem_workspace_load_overdue`.

The three duration fields above are capped at 24 hours; a larger value is rejected at startup.

Environment overrides use the usual form, e.g. `STROEM__WORKSPACE_RELOAD__LOAD_TIMEOUT_SECS=600`.

See [How workspaces are refreshed](/guides/multi-workspace/#how-workspaces-are-refreshed) for the policy these tune.

### `pin_store`

Pinned commits for [git refs](/guides/git-refs/). All fields are optional.

```yaml
pin_store:
  dir: /var/lib/stroem/pins        # default: <temp>/stroem/pins
  keep_recent_per_workspace: 5     # default 5
  claim_load_budget_secs: 20       # default 20
```

| Field | Default | Meaning |
|---|---|---|
| `dir` | `<temp>/stroem/pins` | One bare repository per git workspace plus one checkout per pinned commit. Must be private to ONE server process: startup fails with `pin_store.dir … is in use by another process` if another process holds its lock. Must not be empty. |
| `keep_recent_per_workspace` | `5` | Recently used pinned commits kept per workspace beyond those active jobs still need. At most `1000`. |
| `claim_load_budget_secs` | `20` | How long a worker's claim waits for the pinned commits it needs to load. Past it, the step is released back to `ready` and the load continues in the background. Keep it clearly **below** the workers' `request_timeout_secs` (30 s by default). If a worker gives up first, the server releases that claim too, so the step is only delayed. Between `1` and `86400`. |

The pin store is opened only when at least one git workspace is configured.
It takes a lock on `{dir}/.lock`, so two server processes on one host need
different directories — with the default directory, also two servers
sharing one temporary directory. Startup removes checkouts left half-written
by an interrupted run.

The bare repositories are never garbage-collected. With the default
directory they are lost on a container restart and refilled on demand; the
first pinned run after a restart then fetches again. Fetching a commit, its
checkout and its config load share `workspace_reload.load_timeout_secs`;
listing branches and tags uses `workspace_reload.peek_timeout_secs`.

Environment overrides use the usual form, e.g.
`STROEM__PIN_STORE__DIR=/var/lib/stroem/pins`.

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
| `request_timeout_secs` | No | Timeout of each API request to the server, including step claims (default: 30; workspace downloads have their own longer timeout). Keep it above the server's [`pin_store.claim_load_budget_secs`](#pin_store) (default 20) |
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
