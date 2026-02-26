---
title: Action Types
description: Shell, Docker, Kubernetes, and task action types
---

Actions are the smallest execution unit in Strøm. Each action defines what runs and how.

## Overview

Actions are split into two execution modes:

### Type 1 — Container (run a prepared image)

| Type | Description |
|------|-------------|
| `docker` | Runs a Docker image as-is (no workspace mount) |
| `pod` | Runs an image as a Kubernetes pod (no workspace mount) |

Type 1 actions require `image`. The image's default entrypoint/cmd runs unless overridden. Use this when you have a self-contained image (e.g., DB migrations, deploy tools).

### Type 2 — Shell (commands in a runner environment)

| Type | Runner | Description |
|------|--------|-------------|
| `shell` | `local` (default) | Runs directly on the worker host |
| `shell` + `runner: docker` | Runs in a Docker container with workspace at `/workspace` |
| `shell` + `runner: pod` | Runs as a Kubernetes pod with workspace via init container |

Type 2 actions require `cmd` or `script`. The workspace files are available at `/workspace` (read-only). Use this for build/test/deploy scripts that need your source code.

### Type 3 — Task (sub-job execution)

| Type | Description |
|------|-------------|
| `task` | References another task, creating a child job |

Task actions are dispatched entirely server-side — workers never see them.

:::note
`type: shell` with an `image` field is **rejected** by validation. Use `type: docker` (Type 1) or `type: shell` + `runner: docker` (Type 2) instead.
:::

## Shell actions

### Inline command

```yaml
actions:
  greet:
    type: shell
    cmd: "echo Hello {{ input.name }}"
    input:
      name: { type: string, required: true }
```

### Script file

Scripts are relative to the workspace root:

```yaml
actions:
  deploy:
    type: shell
    script: actions/deploy.sh
    input:
      env: { type: string, default: "staging" }
```

### Environment variables

Actions can declare environment variables. Values support templating:

```yaml
actions:
  deploy:
    type: shell
    script: actions/deploy.sh
    env:
      DEPLOY_ENV: "{{ input.env }}"
      API_KEY: "{{ secret.api_key }}"
    input:
      env: { type: string }
```

## Docker actions (Type 1)

Runs a prepared Docker image as-is. No workspace files are mounted.

```yaml
actions:
  migrate-db:
    type: docker
    image: company/db-migrations:v3
    env:
      DB_URL: "{{ secret.db_url }}"
    # No cmd — image's default entrypoint runs

  deploy:
    type: docker
    image: company/deploy-tool:latest
    cmd: "deploy --env production"

  custom-entrypoint:
    type: docker
    image: company/tool:v2
    entrypoint: ["/app/run"]
    cmd: "--verbose --env staging"
```

## Kubernetes pod actions (Type 1)

Runs a prepared image as a Kubernetes pod. No workspace files are downloaded.

```yaml
actions:
  train-model:
    type: pod
    image: pytorch/pytorch:2.1.0-cuda12.1-cudnn8-runtime
    cmd: "python /app/train.py --epochs 10"
    tags: ["gpu"]
```

## Shell in Docker (Type 2)

Runs shell commands in a Docker container with the workspace mounted at `/workspace` (read-only):

```yaml
actions:
  lint-python:
    type: shell
    runner: docker
    cmd: "pip install ruff && ruff check /workspace"

  run-tests:
    type: shell
    runner: docker
    cmd: "cd /workspace && npm ci && npm test"
```

## Shell in Kubernetes (Type 2)

Runs shell commands as a Kubernetes pod with the workspace downloaded via an init container:

```yaml
actions:
  gpu-test:
    type: shell
    runner: pod
    tags: ["gpu"]
    cmd: "python /workspace/test_gpu.py"
```

## Task actions (Type 3)

Actions of `type: task` reference another task by name. When a step using a task action becomes ready, the server creates a child job that runs the referenced task's full flow.

```yaml
actions:
  run-cleanup:
    type: task
    task: cleanup-resources

tasks:
  cleanup-resources:
    mode: distributed
    input:
      env: { type: string }
    flow:
      cleanup:
        action: cleanup-resources-action
        input:
          env: "{{ input.env }}"

  deploy:
    mode: distributed
    input:
      env: { type: string, default: "staging" }
    flow:
      build:
        action: build-app
      cleanup:
        action: run-cleanup
        depends_on: [build]
        input:
          env: "{{ input.env }}"
```

When the `deploy` task's `cleanup` step becomes ready, the server creates a child job running `cleanup-resources`. The child completes its own flow, and the parent step is marked as completed.

### Rules for task actions

- Must have a `task` field referencing an existing task in the same workspace
- Cannot have `cmd`, `script`, `image`, or `runner` fields
- No worker tags required — task steps are server-dispatched
- Self-referencing tasks are rejected at validation time
- Maximum nesting depth of 10 levels prevents infinite recursion
- Input templates are rendered server-side before creating the child job

### Parent-child relationship

- Child jobs track their parent via `parent_job_id` and `parent_step_name`
- When a child completes, the parent step is marked completed with the child's output
- When a child fails, the parent step is marked as failed
- The parent's orchestrator runs after child completion, promoting downstream steps

## Pod manifest overrides

Actions that run as Kubernetes pods (`type: pod` or `type: shell` + `runner: pod`) support a `manifest` field for injecting arbitrary pod configuration. The value is deep-merged into the generated pod manifest.

### Merge rules

- **Objects**: recursively merged
- **Arrays of objects with `name` field** (e.g., `containers`, `env`): matched by `name` and deep-merged per element; unmatched entries are appended
- **Other arrays and scalars**: replaced entirely

### Examples

**Service account and annotations:**

```yaml
actions:
  deploy:
    type: pod
    image: company/deploy:latest
    cmd: "deploy --env production"
    manifest:
      metadata:
        annotations:
          iam.amazonaws.com/role: my-role
      spec:
        serviceAccountName: my-sa
```

**Resource limits (target the `step` container by name):**

```yaml
actions:
  heavy-build:
    type: pod
    image: node:20
    cmd: "npm run build"
    manifest:
      spec:
        containers:
          - name: step
            resources:
              requests:
                memory: "256Mi"
                cpu: "500m"
              limits:
                memory: "512Mi"
```

**Node selector and tolerations:**

```yaml
actions:
  train-model:
    type: pod
    image: pytorch/pytorch:2.1.0
    tags: ["gpu"]
    cmd: "python train.py"
    manifest:
      spec:
        nodeSelector:
          gpu: "true"
        tolerations:
          - key: "gpu"
            operator: "Exists"
            effect: "NoSchedule"
```

**Adding a sidecar container:**

```yaml
actions:
  test-with-db:
    type: pod
    image: node:20
    cmd: "npm test"
    manifest:
      spec:
        containers:
          - name: postgres-sidecar
            image: postgres:16
            env:
              - name: POSTGRES_PASSWORD
                value: test
```

The `manifest` field is rejected on `type: docker`, `type: task`, and `type: shell` with `runner: local` or `runner: docker`.

## Structured output

Actions can emit structured output by printing a line with the `OUTPUT:` prefix followed by JSON:

```bash
#!/bin/bash
echo "Doing work..."
echo "OUTPUT: {\"status\": \"deployed\", \"version\": \"1.2.3\"}"
```

Only the **last** `OUTPUT: {json}` line is captured. The JSON is parsed and made available to downstream steps via [templating](/guides/templating/).
