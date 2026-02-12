# Strøm

A workflow and task orchestration platform. Define workflows as YAML, execute them via API, CLI, or web UI, and monitor results through live log streaming.

Strøm replaces the need for multiple tools (RunDeck, Windmill, GitHub Actions, Kubernetes CronJobs) with a single, unified platform.

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
#   providers:
#     internal:
#       provider_type: internal
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
```

Workers download workspace files from the server as tarballs and cache them locally in `workspace_cache_dir`. This allows workers to run on different hosts than the server.

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
     │ Public │ Worker │     │
     │  API   │  API   │     │
     └────────┴────────┘     │
                             │
                      ┌──────┴──────┐
                      │ ShellRunner │
                      └─────────────┘
```

- **Server**: Axum HTTP server. Loads workflows from workspace, manages jobs and steps in PostgreSQL, orchestrates DAG execution, stores logs.
- **Worker**: Polls server for ready steps, executes them via ShellRunner, streams logs back.
- **CLI**: HTTP client for triggering tasks, checking status, and viewing logs.

### Crates

| Crate | Description |
|-------|-------------|
| `stroem-common` | Shared types, YAML models, DAG walker, Tera templating, validation |
| `stroem-db` | PostgreSQL layer (sqlx), migrations, repositories |
| `stroem-runner` | Execution backends (ShellRunner for MVP) |
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

**Phase 3 (Multi-Workspace)** -- In progress.
- Multiple named workspaces with folder and git sources
- Workspace-scoped API routes (`/api/workspaces/{ws}/tasks/...`)
- Tarball distribution to workers with ETag caching
- Git workspace sources with SSH key and token auth
- CLI `--workspace` flag and `workspaces` command

Upcoming:
- **Phase 3 (continued)**: Docker/Kubernetes runners
- **Phase 4**: Local execution mode, full k8s pod specs, RBAC, secret resolution

## License

MIT
