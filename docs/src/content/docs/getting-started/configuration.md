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
# Optional: worker recovery settings
# recovery:
#   heartbeat_timeout_secs: 120
#   sweep_interval_secs: 60
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
  - shell
  - docker

# Default image for Type 2 shell-in-container execution
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
| `tags` | No | Tags for step routing (default: `["shell"]`) |
| `runner_image` | No | Default Docker image for Type 2 container steps |
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
