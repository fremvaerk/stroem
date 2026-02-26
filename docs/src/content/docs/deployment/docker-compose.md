---
title: Docker Compose
description: Deploy Strøm with Docker Compose for development and small-scale production
---

Docker Compose is the easiest way to run Strøm. It starts PostgreSQL, the server, and a worker in a single command.

## Quick start

```bash
git clone https://github.com/fremvaerk/stroem.git
cd stroem

# Start all services
docker compose up -d

# Wait for server to be ready
until curl -sf http://localhost:8080/api/workspaces/default/tasks >/dev/null; do sleep 2; done

# Open the web UI
open http://localhost:8080
```

## Services

The default `docker-compose.yml` defines three services:

| Service | Image | Port | Description |
|---------|-------|------|-------------|
| `postgres` | `postgres:16` | 5432 | PostgreSQL database |
| `server` | `stroem-server` | 8080 | HTTP API + web UI |
| `worker` | `stroem-worker` | — | Step executor |

## Configuration

### Server

The server configuration is provided via `server-config.yaml` mounted into the container. Key settings:

```yaml
listen: "0.0.0.0:8080"
db:
  url: "postgres://stroem:stroem@postgres:5432/stroem"
log_storage:
  local_dir: /var/stroem/logs
workspaces:
  default:
    type: folder
    path: /workspace
worker_token: "dev-worker-token-change-in-production"
```

### Worker

The worker configuration is provided via `worker-config.yaml`:

```yaml
server_url: "http://server:8080"
worker_token: "dev-worker-token-change-in-production"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: /var/stroem/workspace-cache
capabilities:
  - shell
```

### Custom workflows

Mount your workflow directory into the server container:

```yaml
services:
  server:
    volumes:
      - ./my-workflows:/workspace
```

## Production considerations

For production use:

1. **Change the worker token** — use a strong random value
2. **Use a managed database** — external PostgreSQL with backups
3. **Enable authentication** — add the `auth` section to server config
4. **Use environment variable overrides** for secrets:
   ```yaml
   services:
     server:
       environment:
         STROEM__DB__URL: "postgres://user:secret@prod-db:5432/stroem"
         STROEM__WORKER_TOKEN: "production-secret-token"
   ```

For larger deployments, consider [Helm / Kubernetes](/deployment/helm/).

## Tear down

```bash
docker compose down -v   # -v removes the database volume
```
