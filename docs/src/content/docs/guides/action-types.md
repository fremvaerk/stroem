---
title: Action Types
description: Script, Docker, Kubernetes, and task action types
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

### Type 2 — Script (commands in a runner environment)

| Type | Runner | Description |
|------|--------|-------------|
| `script` | `local` (default) | Runs directly on the worker host |
| `script` + `runner: docker` | Runs in a Docker container with workspace at `/workspace` |
| `script` + `runner: pod` | Runs as a Kubernetes pod with workspace via init container |

Type 2 actions require `script` or `source`. The workspace files are available at `/workspace` (read-only). Use this for build/test/deploy scripts that need your source code. Script actions support multiple languages via the `language` field.

### Type 3 — Task (sub-job execution)

| Type | Description |
|------|-------------|
| `task` | References another task, creating a child job |

Task actions are dispatched entirely server-side — workers never see them.

:::note
`type: script` with an `image` field is **rejected** by validation. Use `type: docker` (Type 1) or `type: script` + `runner: docker` (Type 2) instead.

For backward compatibility, `cmd:` is still accepted on `type: script` actions, but it is deprecated. Use `script:` for inline code and `source:` for file paths instead.
:::

## Script actions

Script actions run commands or scripts in a runner environment. By default, scripts run as shell commands, but you can use the `language` field to write inline scripts in Python, JavaScript, TypeScript, or Go.

### Inline command

```yaml
actions:
  greet:
    type: script
    script: "echo Hello {{ input.name }}"
    input:
      name: { type: string, required: true }
```

### Script file

Scripts are relative to the workspace root:

```yaml
actions:
  deploy:
    type: script
    source: actions/deploy.sh
    input:
      env: { type: string, default: "staging" }
```

### Environment variables

Actions can declare environment variables. Values support templating:

```yaml
actions:
  deploy:
    type: script
    source: actions/deploy.sh
    env:
      DEPLOY_ENV: "{{ input.env }}"
      API_KEY: "{{ secret.api_key }}"
    input:
      env: { type: string }
```

### Multi-language scripts

Use the `language` field to write inline scripts in languages other than shell. When `language` is set, the `script` content is written to a temporary file and executed with the appropriate interpreter.

| Language | Value | Toolchain preference |
|----------|-------|---------------------|
| Shell | `shell` (default) | `bash > sh` |
| Python | `python` | `uv > python3 > python` |
| JavaScript | `javascript` | `bun > node` |
| TypeScript | `typescript` | `bun > deno` |
| Go | `go` | `go run` |

**Python example:**

```yaml
actions:
  analyze-data:
    type: script
    language: python
    dependencies:
      - pandas
      - requests
    script: |
      import pandas as pd
      import requests

      resp = requests.get("https://api.example.com/data")
      df = pd.DataFrame(resp.json())
      print(f"Rows: {len(df)}")
      print(f'OUTPUT: {{"count": {len(df)}}}')
    input:
      url: { type: string }
```

**JavaScript example:**

```yaml
actions:
  fetch-status:
    type: script
    language: javascript
    script: |
      const resp = await fetch("https://api.example.com/status");
      const data = await resp.json();
      console.log(`Status: ${data.status}`);
      console.log(`OUTPUT: ${JSON.stringify(data)}`);
```

**TypeScript example:**

```yaml
actions:
  generate-report:
    type: script
    language: typescript
    dependencies:
      - zod
    script: |
      import { z } from "zod";
      const Schema = z.object({ name: z.string() });
      const result = Schema.parse({ name: "test" });
      console.log(result);
```

**Go example:**

```yaml
actions:
  compute:
    type: script
    language: go
    script: |
      package main

      import "fmt"

      func main() {
          fmt.Println("OUTPUT: {\"result\": 42}")
      }
```

### Dependencies

The `dependencies` field installs packages before running the script. It requires a `language` other than `shell`.

| Language | Install command |
|----------|----------------|
| Python | `uv pip install --system <deps>` or `pip install <deps>` |
| JavaScript | `bun install <deps>` or `npm install <deps>` |
| TypeScript | `bun install <deps>` or `npm install <deps>` |
| Go | Dependencies are resolved automatically by `go run` |

### Interpreter override

Use the `interpreter` field to override the auto-detected binary:

```yaml
actions:
  legacy-python:
    type: script
    language: python
    interpreter: python3.11
    script: |
      import sys
      print(f"Using Python {sys.version}")
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

## Script in Docker (Type 2)

Runs scripts in a Docker container with the workspace mounted at `/workspace` (read-only):

```yaml
actions:
  lint-python:
    type: script
    runner: docker
    script: "pip install ruff && ruff check /workspace"

  run-tests:
    type: script
    runner: docker
    script: "cd /workspace && npm ci && npm test"

  analyze:
    type: script
    runner: docker
    language: python
    dependencies: [pandas]
    script: |
      import pandas as pd
      df = pd.read_csv("/workspace/data.csv")
      print(f"OUTPUT: {{\"rows\": {len(df)}}}")
```

## Script in Kubernetes (Type 2)

Runs scripts as a Kubernetes pod with the workspace downloaded via an init container:

```yaml
actions:
  gpu-test:
    type: script
    runner: pod
    tags: ["gpu"]
    script: "python /workspace/test_gpu.py"
```

:::note Pending pod timeout
Pods that remain in `Pending` state for more than 10 minutes are automatically terminated and the step is marked as failed. This prevents jobs from hanging indefinitely when pods can't be scheduled (e.g., insufficient resources, node affinity failures, image pull errors). The error message includes the pod's status reason when available.
:::

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
- Cannot have `script`, `source`, `image`, or `runner` fields
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

Actions that run as Kubernetes pods (`type: pod` or `type: script` + `runner: pod`) support a `manifest` field for injecting arbitrary pod configuration. The value is deep-merged into the generated pod manifest.

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

The `manifest` field is rejected on `type: docker`, `type: task`, and `type: script` with `runner: local` or `runner: docker`.

## Structured output

Actions can emit structured output by printing a line with the `OUTPUT:` prefix followed by JSON:

```bash
#!/bin/bash
echo "Doing work..."
echo "OUTPUT: {\"status\": \"deployed\", \"version\": \"1.2.3\"}"
```

Only the **last** `OUTPUT: {json}` line is captured. The JSON is parsed and made available to downstream steps via [templating](/guides/templating/).
