# Strøm

A workflow and task orchestration platform. Define workflows as YAML, execute them via API or CLI, and monitor results through logs.

Strøm replaces the need for multiple tools (RunDeck, Windmill, GitHub Actions, Kubernetes CronJobs) with a single, unified platform.

## Quickstart (Docker Compose)

Prerequisites: Docker, curl, jq.

```bash
# Start all services (Postgres, server, worker)
docker compose up -d

# Wait for server to be ready
until curl -sf http://localhost:8080/api/tasks >/dev/null; do sleep 2; done

# List available tasks
curl -s http://localhost:8080/api/tasks | jq .

# Run the hello-world task
curl -s -X POST http://localhost:8080/api/tasks/hello-world/execute \
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

Prerequisites: Rust (latest stable), PostgreSQL, Docker (for integration tests).

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

### Configuration

**Server** (`server-config.yaml`):

```yaml
listen: "0.0.0.0:8080"
db:
  url: "postgres://stroem:stroem@localhost:5432/stroem"
log_storage:
  local_dir: /tmp/stroem/logs
workspace:
  source_type: folder
  path: ./workspace
worker_token: "dev-worker-token-change-in-production"
```

**Worker** (`worker-config.yaml`):

```yaml
server_url: "http://localhost:8080"
worker_token: "dev-worker-token-change-in-production"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_dir: "./workspace"
capabilities:
  - shell
```

## CLI

The `stroem` CLI communicates with the server over HTTP.

```bash
# Build the CLI
cargo build -p stroem-cli

# Set server URL (default: http://localhost:8080)
export STROEM_URL=http://localhost:8080

# List tasks
stroem tasks

# Trigger a task
stroem trigger hello-world --input '{"name": "CLI"}'

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
| `stroem-server` | Axum API server, orchestrator, workspace loader, log storage |
| `stroem-worker` | Worker process: polls, executes, streams logs |
| `stroem-cli` | CLI tool: validate, trigger, status, logs, tasks, jobs |

## API Reference

See [docs/api-reference.md](docs/api-reference.md) for the full API documentation.

### Public API

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/tasks` | List all tasks |
| `GET` | `/api/tasks/{name}` | Get task detail |
| `POST` | `/api/tasks/{name}/execute` | Trigger task execution |
| `GET` | `/api/jobs` | List jobs |
| `GET` | `/api/jobs/{id}` | Get job detail with steps |
| `GET` | `/api/jobs/{id}/logs` | Get job logs |

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
| `POST` | `/worker/jobs/{id}/complete` | Complete local-mode job |

## Testing

```bash
# Unit tests
cargo test --workspace

# Integration tests (requires Docker)
cargo test -p stroem-db

# E2E tests (requires Docker Compose)
./tests/e2e.sh

# Lint
cargo clippy --workspace -- -D warnings

# Format check
cargo fmt --check --all
```

## Project Status

**Phase 1 (MVP)** -- Complete. End-to-end workflow execution via API and CLI.

Upcoming phases:
- **Phase 2**: React UI + authentication (JWT, OIDC)
- **Phase 3**: Multi-workspace, Docker/Kubernetes runners, Git workspace sources
- **Phase 4**: Local execution mode, full k8s pod specs, RBAC, secret resolution

## License

MIT
