---
title: CLI
description: Command-line interface reference
---

Strøm provides two CLI binaries:

- **`stroem`** — Local workspace tool for exploring and running tasks without a server
- **`stroem-api`** — Remote server client for triggering jobs and querying a running Strøm server

## `stroem` (Local)

Local workspace commands. No server required.

```bash
# Set the workspace path (default: current directory)
stroem --path /path/to/workspace <command>
```

### `run`

Run a task locally without a server. Loads the workspace, walks the task's DAG, and executes each step via the local shell runner. Only `type: script` with `runner: local` (default) is supported — docker, pod, task, agent, and approval steps are rejected upfront.

```bash
# Run a task in the current directory
stroem run my-task

# Run from a specific workspace path
stroem run my-task --path /path/to/workspace

# Run with input
stroem run deploy --input '{"env": "staging"}'
```

Supports:
- DAG dependency ordering
- Template rendering (`{{ input.* }}`, `{{ step.output.* }}`, `{{ secret.* }}`)
- `when` conditions (skip steps based on expressions)
- `for_each` loops (iterate over arrays)
- Cascade-skip (downstream steps skipped when all dependencies are skipped)
- `Ctrl+C` graceful cancellation
- `OUTPUT: {json}` parsing for step outputs

Limitations:
- Steps execute sequentially, even when the DAG allows parallelism
- `for_each` iterations always run sequentially regardless of the `sequential` setting
- Only `type: script` with local runner is supported — docker, pod, task, agent, and approval steps are rejected

### `validate`

Validate workflow YAML files before deploying.

```bash
# Validate current directory
stroem validate

# Validate a specific path
stroem validate /path/to/workspace/.workflows/
```

The validator checks:
- YAML syntax and structure
- Action type validity
- Runner field validity
- Flow step and dependency references
- DAG cycle detection
- Trigger cron expression syntax
- Hook action references

### `tasks`

List all tasks in the workspace.

```bash
stroem tasks
stroem --path /path/to/workspace tasks
```

### `actions`

List all actions in the workspace.

```bash
stroem actions
```

### `triggers`

List all triggers (scheduler and webhook) in the workspace.

```bash
stroem triggers
```

### `inspect`

Show detailed information about a single task: input schema, flow steps with dependencies, hooks.

```bash
stroem inspect deploy-pipeline
```

## `stroem-api` (Remote)

Remote server commands. Requires a running Strøm server.

```bash
# Set the server URL (default: http://localhost:8080)
export STROEM_URL=http://localhost:8080

# Set authentication token (optional)
export STROEM_TOKEN=strm_your_api_key
```

### `trigger`

Execute a task and create a new job.

```bash
# Trigger with default input
stroem-api trigger hello-world

# Trigger with input
stroem-api trigger hello-world --input '{"name": "CLI"}'

# Trigger in a specific workspace
stroem-api trigger etl-pipeline --workspace data-team --input '{"date": "2025-01-01"}'
```

### `status`

Check the status of a job.

```bash
stroem-api status <job-id>
```

### `logs`

View the logs of a job.

```bash
stroem-api logs <job-id>
```

### `jobs`

List recent jobs.

```bash
# List last 10 jobs
stroem-api jobs --limit 10
```

### `tasks`

List tasks from the server (across workspaces).

```bash
# List all tasks
stroem-api tasks

# Filter by workspace
stroem-api tasks --workspace data-team
```

### `cancel`

Cancel a running or pending job.

```bash
stroem-api cancel <job-id>
```

### `workspaces`

List all configured workspaces on the server.

```bash
stroem-api workspaces
```
