# Strøm

A workflow and task orchestration platform. Define workflows as YAML, execute them via API, CLI, or web UI, and monitor results through live log streaming.

Strøm replaces the need for multiple tools (RunDeck, Windmill, GitHub Actions, Kubernetes CronJobs) with a single, unified platform.

## Installation

### Pre-built binaries

Download the latest release from [GitHub Releases](https://github.com/fremvaerk/stroem/releases). Binaries are available for:

| Platform | Archive |
|----------|---------|
| Linux (x86_64) | `stroem-{server,worker,cli}-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (ARM64) | `stroem-{server,worker,cli}-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `stroem-{server,worker,cli}-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `stroem-{server,worker,cli}-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `stroem-{server,worker,cli}-x86_64-pc-windows-msvc.zip` |

```bash
# Example: install the CLI on macOS ARM64
curl -fsSL https://github.com/fremvaerk/stroem/releases/latest/download/stroem-cli-aarch64-apple-darwin.tar.gz \
  | tar xz -C /usr/local/bin
```

### Docker images

Multi-arch images (amd64 + arm64) are published to GHCR:

```bash
docker pull ghcr.io/fremvaerk/stroem-server:latest
docker pull ghcr.io/fremvaerk/stroem-worker:latest
docker pull ghcr.io/fremvaerk/stroem-runner:latest  # base image for shell-in-container steps
```

## Quickstart (Docker Compose)

Prerequisites: Docker, curl, jq.

```bash
# Start all services (Postgres, server, worker)
docker compose up -d

# Wait for server to be ready
until curl -sf http://localhost:8080/api/workspaces/default/tasks >/dev/null; do sleep 2; done

# List workspaces
curl -s http://localhost:8080/api/workspaces | jq .

# List available tasks in the default workspace
curl -s http://localhost:8080/api/workspaces/default/tasks | jq .

# Run the hello-world task
curl -s -X POST http://localhost:8080/api/workspaces/default/tasks/hello-world/execute \
  -H "Content-Type: application/json" \
  -d '{"input": {"name": "World"}}' | jq .

# Check job status (replace JOB_ID with the actual ID from above)
curl -s http://localhost:8080/api/jobs/JOB_ID | jq .

# View logs
curl -s http://localhost:8080/api/jobs/JOB_ID/logs | jq -r .logs

# Tear down
docker compose down -v
```

## Local Development

Prerequisites: Rust (latest stable), Bun, PostgreSQL, Docker (for integration tests).

```bash
# Start Postgres (via docker compose or locally)
docker compose up -d postgres

# Build all crates
cargo build --workspace

# Run unit tests
cargo test --workspace

# Run integration tests (requires Docker for testcontainers)
cargo test -p stroem-db

# Start the server
STROEM_CONFIG=server-config.yaml cargo run -p stroem-server

# Start a worker (in another terminal)
STROEM_CONFIG=worker-config.yaml cargo run -p stroem-worker
```

### Frontend Development

```bash
# Install frontend dependencies
cd ui && bun install

# Start dev server (proxies API to backend on :8080)
bun run dev

# Build for production (outputs to crates/stroem-server/static/)
bun run build

# Build and serve embedded UI
cd ui && bun run build && cd .. && cargo run -p stroem-server
# Visit http://localhost:8080
```

### Configuration

**Server** (`server-config.yaml`):

```yaml
listen: "0.0.0.0:8080"
db:
  url: "postgres://stroem:stroem@localhost:5432/stroem"
log_storage:
  local_dir: /tmp/stroem/logs
workspaces:
  default:
    type: folder
    path: ./workspace
  # Add more workspaces:
  # data-team:
  #   type: git
  #   url: https://github.com/org/data-workflows.git
  #   ref: main
  #   poll_interval_secs: 60
worker_token: "dev-worker-token-change-in-production"
# Optional: enable authentication
# auth:
#   jwt_secret: "your-jwt-secret"
#   refresh_secret: "your-refresh-secret"
#   base_url: "https://stroem.company.com"  # Required for OIDC
#   providers:
#     internal:
#       provider_type: internal
#     # OIDC SSO provider example:
#     # google:
#     #   provider_type: oidc
#     #   display_name: "Google"
#     #   issuer_url: "https://accounts.google.com"
#     #   client_id: "your-client-id.apps.googleusercontent.com"
#     #   client_secret: "your-client-secret"
#   initial_user:
#     email: admin@stroem.local
#     password: admin
```

**Worker** (`worker-config.yaml`):

```yaml
server_url: "http://localhost:8080"
worker_token: "dev-worker-token-change-in-production"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: "/tmp/stroem-workspace"
capabilities:
  - shell

# Optional: enable Docker runner (requires Docker daemon access)
# docker: {}

# Optional: enable Kubernetes runner (requires in-cluster or kubeconfig access)
# kubernetes:
#   namespace: stroem-jobs
#   init_image: curlimages/curl:latest  # downloads workspace tarball
```

Workers download workspace files from the server as tarballs and cache them locally in `workspace_cache_dir`. This allows workers to run on different hosts than the server.

#### Container Runners

By default, the worker only supports `type: shell` actions (runs directly on the host). To run actions inside **Docker containers** or **Kubernetes pods**, enable the respective runners:

**Docker runner** — Build the worker with the `docker` feature and add `docker: {}` to the worker config. The worker needs access to a Docker daemon (local socket or DinD sidecar in K8s).

```bash
cargo build -p stroem-worker --features docker
```

**Kubernetes runner** — Build the worker with the `kubernetes` feature and add a `kubernetes:` section to the worker config. The worker creates pods in the configured namespace. An init container downloads the workspace tarball from the server before the step runs.

```bash
cargo build -p stroem-worker --features kubernetes
```

Both features can be enabled simultaneously:

```bash
cargo build -p stroem-worker --features docker,kubernetes
```

See [docs/workflow-authoring.md](docs/workflow-authoring.md) for how to write workflows using `type: docker` and `type: pod` actions.

## CLI

The `stroem` CLI communicates with the server over HTTP.

```bash
# Build the CLI
cargo build -p stroem-cli

# Set server URL (default: http://localhost:8080)
export STROEM_URL=http://localhost:8080

# List workspaces
stroem workspaces

# List tasks (all workspaces)
stroem tasks

# List tasks in a specific workspace
stroem tasks --workspace data-team

# Trigger a task
stroem trigger hello-world --input '{"name": "CLI"}'

# Trigger a task in a specific workspace
stroem trigger etl-pipeline --workspace data-team --input '{"date": "2025-01-01"}'

# Check job status
stroem status <job-id>

# View job logs
stroem logs <job-id>

# List recent jobs
stroem jobs --limit 10

# Validate workflow YAML files
stroem validate workspace/.workflows/
```

## Kubernetes Deployment (Helm)

A Helm chart is provided at `helm/stroem/`. It deploys the server and worker(s) with proper secret management.

```bash
# Basic install
helm install stroem ./helm/stroem \
  --set database.url="postgres://stroem:pw@postgres:5432/stroem" \
  --set workerToken="my-secure-token"

# With auth enabled
helm install stroem ./helm/stroem \
  --set database.url="postgres://stroem:pw@postgres:5432/stroem" \
  --set workerToken="my-secure-token" \
  --set auth.enabled=true \
  --set auth.jwtSecret="jwt-secret-here" \
  --set auth.refreshSecret="refresh-secret-here"

# With Kubernetes runner (steps run as K8s pods)
helm install stroem ./helm/stroem \
  --set database.url="postgres://stroem:pw@postgres:5432/stroem" \
  --set workerToken="my-secure-token" \
  --set worker.kubernetes.enabled=true \
  --set worker.kubernetes.namespace=stroem-jobs

# With Docker runner (steps run in Docker containers via DinD sidecar)
helm install stroem ./helm/stroem \
  --set database.url="postgres://stroem:pw@postgres:5432/stroem" \
  --set workerToken="my-secure-token" \
  --set worker.dind.enabled=true
```

Key Helm values:

| Value | Description | Default |
|-------|-------------|---------|
| `server.config.workspaces` | Workspace map (folder/git sources) | `default: { type: folder, path: /workspace }` |
| `worker.replicas` | Number of worker pods | `2` |
| `worker.kubernetes.enabled` | Enable KubeRunner | `false` |
| `worker.kubernetes.namespace` | Namespace for step pods | `stroem-jobs` |
| `worker.dind.enabled` | Enable DinD sidecar for DockerRunner | `false` |
| `auth.enabled` | Enable JWT authentication | `false` |
| `auth.providers` | OIDC provider list | `[]` |

Secrets (database URL, worker token, JWT secrets) are stored in a Kubernetes Secret and injected into config files at startup via an init container.

## Writing Workflows

Workflows are defined in YAML files under `workspace/.workflows/`. See [docs/workflow-authoring.md](docs/workflow-authoring.md) for the full guide.

### Quick Example

```yaml
actions:
  greet:
    type: shell
    cmd: "echo Hello {{ input.name }} && echo 'OUTPUT: {\"greeting\": \"Hello {{ input.name }}\"}'"
    input:
      name: { type: string, required: true }

  shout:
    type: shell
    cmd: "echo {{ input.message }} | tr '[:lower:]' '[:upper:]'"
    input:
      message: { type: string, required: true }

tasks:
  hello-world:
    mode: distributed
    input:
      name: { type: string, default: "World" }
    flow:
      say-hello:
        action: greet
        input:
          name: "{{ input.name }}"
      shout-it:
        action: shout
        depends_on: [say-hello]
        input:
          message: "{{ say_hello.output.greeting }}"
```

Key concepts:
- **Actions** are reusable execution units (shell commands, scripts)
- **Tasks** compose actions into a DAG (directed acyclic graph) of steps
- **Steps** can pass data via `OUTPUT: {json}` lines in stdout
- **Templating** uses Tera syntax (`{{ variable }}`) for dynamic values

## Architecture

```
                    ┌─────────────────┐
                    │    PostgreSQL    │
                    └────────┬────────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
         ┌────┴────┐   ┌────┴────┐   ┌────┴────┐
         │ Server  │   │ Worker  │   │   CLI   │
         │ (Axum)  │◄──┤ (Poll)  │   │(reqwest)│
         └────┬────┘   └────┬────┘   └─────────┘
              │              │
     ┌────────┼────────┐     │
     │ Public │ Worker │     ├──── ShellRunner (host)
     │  API   │  API   │     ├──── DockerRunner (bollard)
     └────────┴────────┘     └──── KubeRunner (kube)
```

- **Server**: Axum HTTP server. Loads workflows from workspace, manages jobs and steps in PostgreSQL, orchestrates DAG execution, stores logs.
- **Worker**: Polls server for ready steps, dispatches to the appropriate runner based on `type`/`image`, streams logs back.
- **CLI**: HTTP client for triggering tasks, checking status, and viewing logs.

### Crates

| Crate | Description |
|-------|-------------|
| `stroem-common` | Shared types, YAML models, DAG walker, Tera templating, validation |
| `stroem-db` | PostgreSQL layer (sqlx), migrations, repositories |
| `stroem-runner` | Execution backends: ShellRunner, DockerRunner (optional), KubeRunner (optional) |
| `stroem-server` | Axum API server, orchestrator, workspace loader, log storage, embedded UI |
| `stroem-worker` | Worker process: polls, executes, streams logs |
| `stroem-cli` | CLI tool: validate, trigger, status, logs, tasks, jobs |

## API Reference

See [docs/api-reference.md](docs/api-reference.md) for the full API documentation.

### Public API

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/workspaces` | List all workspaces |
| `GET` | `/api/workspaces/{ws}/tasks` | List tasks in workspace |
| `GET` | `/api/workspaces/{ws}/tasks/{name}` | Get task detail |
| `POST` | `/api/workspaces/{ws}/tasks/{name}/execute` | Trigger task execution |
| `GET` | `/api/jobs` | List jobs |
| `GET` | `/api/jobs/{id}` | Get job detail with steps |
| `GET` | `/api/jobs/{id}/logs` | Get job logs |
| `WS` | `/api/jobs/{id}/logs/stream` | WebSocket live log stream |
| `POST` | `/api/auth/login` | Email/password login |
| `POST` | `/api/auth/refresh` | Refresh JWT token |
| `POST` | `/api/auth/logout` | Revoke refresh token |
| `GET` | `/api/auth/me` | Get current user info |

### Worker API

Authenticated via `Authorization: Bearer <worker_token>`.

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/worker/register` | Register worker |
| `POST` | `/worker/heartbeat` | Worker heartbeat |
| `POST` | `/worker/jobs/claim` | Claim next ready step |
| `POST` | `/worker/jobs/{id}/steps/{step}/start` | Mark step running |
| `POST` | `/worker/jobs/{id}/steps/{step}/complete` | Report step result |
| `POST` | `/worker/jobs/{id}/logs` | Push log chunk |
| `GET` | `/worker/workspace/{ws}.tar.gz` | Download workspace tarball |
| `POST` | `/worker/jobs/{id}/complete` | Complete local-mode job |

## Testing

```bash
# Unit tests
cargo test --workspace

# Integration tests (requires Docker)
cargo test -p stroem-db

# E2E tests (requires Docker Compose)
./tests/e2e.sh

# Playwright browser E2E tests
cd ui && bunx playwright test

# Playwright in Docker
docker compose -f docker-compose.yml -f docker-compose.test.yml \
  up --build --abort-on-container-exit playwright

# Lint
cargo clippy --workspace -- -D warnings

# Format check
cargo fmt --check --all
```

## Project Status

**Phase 1 (MVP)** -- Complete. End-to-end workflow execution via API and CLI.

**Phase 2a (Auth + WebSocket)** -- Complete. JWT authentication backend + WebSocket log streaming.
- JWT access tokens (15min) + refresh token rotation (30 day)
- Internal auth provider (email/password with argon2id hashing)
- Auth is optional -- existing routes work without auth configured
- WebSocket endpoint for real-time log streaming with backfill

**Phase 2b (React UI)** -- Complete. Web UI embedded in the Rust binary.
- React 19 + TypeScript + Vite + Tailwind v4 + shadcn/ui
- Pages: Login, Dashboard, Tasks, Task Detail (with run form), Jobs, Job Detail (with live logs)
- Auto-detects auth configuration; login page only shown when auth is enabled
- UI embedded in server binary via rust-embed, served with SPA fallback
- Playwright E2E browser tests

**Phase 3 (Multi-Workspace + Container Runners)** -- Complete.
- Multiple named workspaces with folder and git sources
- Workspace-scoped API routes (`/api/workspaces/{ws}/tasks/...`)
- Tarball distribution to workers with ETag caching
- Git workspace sources with SSH key and token auth
- CLI `--workspace` flag and `workspaces` command
- DockerRunner: run steps in Docker containers (via bollard, optional feature)
- KubeRunner: run steps as Kubernetes pods with workspace init container (via kube, optional feature)
- Production Helm chart with init-container secret injection, DinD sidecar, RBAC

Upcoming:
- **Phase 4**: Local execution mode, full k8s pod specs, RBAC, secret resolution

## License

MIT
