---
title: CLI
description: Command-line interface reference
---

The `stroem` CLI communicates with the server over HTTP.

## Setup

```bash
# Set the server URL (default: http://localhost:8080)
export STROEM_URL=http://localhost:8080
```

## Commands

### `workspaces`

List all configured workspaces.

```bash
stroem workspaces
```

### `tasks`

List tasks across all workspaces or filter by workspace.

```bash
# List all tasks
stroem tasks

# Filter by workspace
stroem tasks --workspace data-team
```

### `trigger`

Execute a task and create a new job.

```bash
# Trigger with default input
stroem trigger hello-world

# Trigger with input
stroem trigger hello-world --input '{"name": "CLI"}'

# Trigger in a specific workspace
stroem trigger etl-pipeline --workspace data-team --input '{"date": "2025-01-01"}'
```

### `status`

Check the status of a job.

```bash
stroem status <job-id>
```

### `logs`

View the logs of a job.

```bash
stroem logs <job-id>
```

### `jobs`

List recent jobs.

```bash
# List last 10 jobs
stroem jobs --limit 10
```

### `validate`

Validate workflow YAML files before deploying.

```bash
# Validate a single file
stroem validate workspace/.workflows/deploy.yaml

# Validate all files in a directory
stroem validate workspace/.workflows/
```

The validator checks:
- YAML syntax and structure
- Action type validity
- Runner field validity
- Flow step and dependency references
- DAG cycle detection
- Trigger cron expression syntax
- Hook action references
