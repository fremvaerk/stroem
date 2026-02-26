---
title: Quickstart
description: Get Strøm running in minutes with Docker Compose
---

This guide gets you from zero to running your first workflow in under five minutes using Docker Compose.

## Prerequisites

- Docker and Docker Compose
- `curl` and `jq` (for API calls)

## Start the stack

```bash
# Clone the repository
git clone https://github.com/fremvaerk/stroem.git
cd stroem

# Start all services (Postgres, server, worker)
docker compose up -d

# Wait for the server to be ready
until curl -sf http://localhost:8080/api/workspaces/default/tasks >/dev/null; do sleep 2; done
```

This starts three containers:
- **PostgreSQL** — stores jobs, steps, and worker state
- **Server** — HTTP API, workflow engine, embedded web UI
- **Worker** — polls for work and executes steps

## Run your first workflow

The default workspace includes a `hello-world` task. Let's trigger it:

```bash
# List available tasks
curl -s http://localhost:8080/api/workspaces/default/tasks | jq .

# Run the hello-world task
curl -s -X POST http://localhost:8080/api/workspaces/default/tasks/hello-world/execute \
  -H "Content-Type: application/json" \
  -d '{"input": {"name": "World"}}' | jq .
```

The response contains a `job_id`. Use it to check status and view logs:

```bash
# Check job status (replace JOB_ID)
curl -s http://localhost:8080/api/jobs/JOB_ID | jq .

# View logs
curl -s http://localhost:8080/api/jobs/JOB_ID/logs | jq -r .logs
```

## Open the web UI

Visit [http://localhost:8080](http://localhost:8080) to see the web UI. From there you can:

- Browse tasks and trigger them with a form
- Watch jobs execute in real-time with live log streaming
- View step dependencies as an interactive graph

## Explore the workflow file

The hello-world task is defined in `workspace/.workflows/hello.yaml`:

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
- **Actions** define reusable commands or scripts
- **Tasks** compose actions into a DAG of steps
- Steps pass data via `OUTPUT: {json}` lines in stdout
- Templates use [Tera](https://keats.github.io/tera/) syntax (`{{ variable }}`)

## Clean up

```bash
docker compose down -v
```

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

- **Server**: Loads workflows from workspaces, manages jobs and steps in PostgreSQL, orchestrates DAG execution, streams logs via WebSocket, serves the embedded web UI.
- **Worker**: Polls the server for ready steps, dispatches to the appropriate runner (shell, Docker, or Kubernetes), streams logs back.
- **CLI**: HTTP client for triggering tasks, checking status, validating workflows, and viewing logs.

## Next steps

- [Configuration](/getting-started/configuration/) — server and worker config files
- [Workflow Basics](/guides/workflow-basics/) — YAML structure in depth
- [Action Types](/guides/action-types/) — shell, Docker, Kubernetes, and task actions
