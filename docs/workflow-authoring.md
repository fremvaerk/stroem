# Workflow Authoring Guide

This guide covers how to write workflow YAML files for Strøm.

## File Location

Workflow files go in the `.workflows/` directory of each workspace and must have a `.yaml` or `.yml` extension. The server loads all files from this directory on startup.

A single YAML file can contain multiple actions and tasks.

For the default folder workspace, files are at `workspace/.workflows/`. For git-sourced workspaces, the repository must contain a `.workflows/` directory at the root.

## YAML Structure

```yaml
actions:
  <action-name>:
    type: shell
    cmd: "..."
    # or
    script: actions/my-script.sh
    input:
      <param-name>: { type: string, required: true }
      <param-name>: { type: string, default: "value" }

tasks:
  <task-name>:
    mode: distributed
    folder: <optional-folder-path>
    input:
      <param-name>: { type: string, default: "value" }
    flow:
      <step-name>:
        action: <action-name>
        depends_on: [<other-step>]
        input:
          <param>: "{{ input.param }}"
```

## Actions

Actions are the smallest execution unit. Each action defines a command or script that runs on a worker.

### Action Types

Actions are split into two execution modes:

**Type 1 — Container (run a prepared image):**

| Type | Description |
|------|-------------|
| `docker` | Runs user's prepared Docker image as-is (no workspace mount) |
| `pod` | Runs user's prepared image as a Kubernetes pod (no workspace mount) |

Type 1 actions require `image`. The image's default entrypoint/cmd runs unless overridden with `entrypoint` and/or `cmd`. Use this when you have a self-contained image (e.g. DB migrations, deploy tools).

**Type 2 — Shell (commands in a runner environment):**

| Type | Runner | Description |
|------|--------|-------------|
| `shell` | `local` (default) | Runs directly on the worker host |
| `shell` + `runner: docker` | Runs in a Docker container with workspace mounted at `/workspace` |
| `shell` + `runner: pod` | Runs as a Kubernetes pod with workspace downloaded via init container |

Type 2 actions require `cmd` or `script`. The workspace files are available at `/workspace` (read-only). Use this for build/test/deploy scripts that need your source code.

### Inline command (shell)

```yaml
actions:
  greet:
    type: shell
    cmd: "echo Hello {{ input.name }}"
    input:
      name: { type: string, required: true }
```

### Script file (shell)

Scripts are relative to the workspace root.

```yaml
actions:
  deploy:
    type: shell
    script: actions/deploy.sh
    input:
      env: { type: string, default: "staging" }
```

### Type 1: Docker container action

Runs a prepared Docker image as-is. No workspace files are mounted. Use this for self-contained images like DB migrations, deploy tools, or pre-built applications.

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

### Type 1: Kubernetes pod action

Runs a prepared image as a Kubernetes pod. No workspace files are downloaded.

```yaml
actions:
  train-model:
    type: pod
    image: pytorch/pytorch:2.1.0-cuda12.1-cudnn8-runtime
    cmd: "python /app/train.py --epochs 10"
    tags: ["gpu"]
```

### Type 2: Shell in Docker

Runs shell commands in a Docker container with the workspace mounted at `/workspace` (read-only). Requires `runner: docker` on the action.

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
    input:
      test_suite: { type: string, default: "unit" }
```

### Type 2: Shell in Kubernetes pod

Runs shell commands as a Kubernetes pod with the workspace downloaded via an init container.

```yaml
actions:
  gpu-test:
    type: shell
    runner: pod
    tags: ["gpu"]
    cmd: "python /workspace/test_gpu.py"
```

### Configuring Container Runners

Container runners (Docker and Kubernetes) are optional features that must be enabled at build time and configured in the worker config.

**Docker runner prerequisites:**
1. Build the worker with the `docker` feature: `cargo build -p stroem-worker --features docker`
2. Add `docker: {}` to the worker config
3. The worker needs Docker daemon access (local socket, or DinD sidecar in K8s via `worker.dind.enabled=true` in Helm)

**Kubernetes runner prerequisites:**
1. Build the worker with the `kubernetes` feature: `cargo build -p stroem-worker --features kubernetes`
2. Add a `kubernetes:` section to the worker config:
   ```yaml
   kubernetes:
     namespace: stroem-jobs
     init_image: curlimages/curl:latest  # optional, default
   ```
3. The worker needs in-cluster credentials or a kubeconfig with permissions to create/get/delete pods in the target namespace
4. The server must be reachable from inside the pod (the init container downloads workspace tarballs from the server)

**Worker config example with both runners:**

```yaml
server_url: "http://stroem-server:8080"
worker_token: "your-token"
worker_name: "worker-1"
max_concurrent: 4
poll_interval_secs: 2
workspace_cache_dir: /var/stroem/workspace-cache

# Tags declare what this worker can run (replaces capabilities)
tags:
  - shell
  - docker
  - kubernetes

# Default image for Type 2 shell-in-container execution
runner_image: "ghcr.io/myorg/stroem-runner:latest"

docker: {}

kubernetes:
  namespace: stroem-jobs
```

### Worker tags

Tags control which steps a worker can claim. Each step computes `required_tags` based on its action type and runner:

| Action | Runner | Required tags |
|--------|--------|--------------|
| `shell` | `local` (default) | `["shell"]` |
| `shell` | `docker` | `["docker"]` |
| `shell` | `pod` | `["kubernetes"]` |
| `docker` | — | `["docker"]` |
| `pod` | — | `["kubernetes"]` |
| `task` | — | `[]` (server-dispatched) |

Actions can add extra tags via the `tags` field (e.g., `tags: ["gpu"]`). A worker claims a step only when all required tags are present in the worker's tag set.

For backward compatibility, `capabilities` still works — if `tags` is not set, `capabilities` is used as the tag set.

**Helm chart:** When deploying via Helm, set `worker.kubernetes.enabled=true` and/or `worker.dind.enabled=true` to automatically configure the worker. The worker Docker image must be built with the corresponding features:

```bash
# Build worker with both container runners
docker build -f Dockerfile.worker --build-arg FEATURES="docker,kubernetes" .
```

### Action Type: task (Sub-Job Execution)

Actions of `type: task` reference another task by name. When a step using a task action becomes ready, the server creates a child job that runs the referenced task's full flow. Workers never see task steps — they are dispatched entirely server-side.

```yaml
actions:
  run-cleanup:
    type: task
    task: cleanup-resources

  cleanup-resources-action:
    type: shell
    cmd: "echo 'Cleaning up...'"

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

In this example, when the `deploy` task's `cleanup` step becomes ready (after `build` completes), the server creates a child job running the `cleanup-resources` task. The child job executes its own flow steps, and when complete, the parent's `cleanup` step is marked as completed.

**Rules for task actions:**
- Must have `task` field referencing an existing task in the same workspace
- Cannot have `cmd`, `script`, `image`, or `runner` fields
- No worker tags required — task steps are server-dispatched
- Self-referencing tasks (direct or via hooks) are rejected at validation time
- Maximum nesting depth of 10 levels prevents infinite recursion
- Step input templates are rendered server-side before creating the child job

**Task actions in hooks:**

Task actions also work in `on_success` and `on_error` hooks, creating a full child job for the referenced task instead of a single-step hook job:

```yaml
tasks:
  deploy:
    flow:
      deploy:
        action: deploy-app
    on_error:
      - action: run-cleanup
        input:
          env: "{{ hook.workspace }}"
```

**Parent-child relationship:**
- Child jobs track their parent via `parent_job_id` and `parent_step_name`
- When a child job completes, the parent step is marked as completed with the child's output
- When a child job fails, the parent step is marked as failed
- The parent's orchestrator runs after child completion, promoting any downstream steps

### Environment variables

Actions can declare environment variables. Values support templating.

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

### Structured output

Actions can emit structured output by printing a line with the `OUTPUT:` prefix followed by JSON:

```bash
#!/bin/bash
echo "Doing work..."
echo "OUTPUT: {\"status\": \"deployed\", \"version\": \"1.2.3\"}"
```

Only the **last** `OUTPUT: {json}` line is captured. The JSON is parsed and made available to downstream steps via templating.

## Tasks

Tasks compose actions into a DAG (directed acyclic graph) of steps.

### Basic task

```yaml
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
```

### Step dependencies

Use `depends_on` to define ordering. Steps without dependencies run as soon as a worker claims them. Steps with dependencies wait until all listed steps complete.

```yaml
tasks:
  deploy-pipeline:
    mode: distributed
    input:
      env: { type: string, default: "staging" }
    flow:
      health-check:
        action: check-status
      deploy-app:
        action: deploy
        depends_on: [health-check]
        input:
          env: "{{ input.env }}"
      send-notification:
        action: notify
        depends_on: [deploy-app]
        input:
          env: "{{ input.env }}"
          status: "success"
```

This creates a linear pipeline: `health-check` -> `deploy-app` -> `send-notification`.

### Handling step failures

By default, when a step fails, all downstream steps that depend on it are automatically **skipped**. The job is marked as `failed` once all steps reach a terminal state.

If you want a step to run even when its dependency fails (e.g., cleanup steps, notifications), use `continue_on_failure: true`:

```yaml
flow:
  deploy:
    action: deploy-app
  notify:
    action: send-notification
    depends_on: [deploy]
    continue_on_failure: true
    input:
      status: "deploy finished"
```

In this example, `notify` runs regardless of whether `deploy` succeeds or fails.

The `continue_on_failure` flag has dual semantics (similar to GitHub Actions' `continue-on-error`):

1. **Dependency tolerance**: The step runs even if its dependencies fail (as shown above).
2. **Job tolerance**: If the step itself fails, its failure is considered *tolerable* -- the job can still be marked `completed` as long as all non-tolerable steps succeed.

For example, if a step with `continue_on_failure: true` fails but every other step succeeds, the job status is `completed` (not `failed`). The UI shows a warning banner listing the failed tolerable steps. If any step *without* the flag fails, the job is marked `failed` as usual.

### Parallel execution

Steps without mutual dependencies run in parallel:

```yaml
flow:
  checkout:
    action: git-clone
  setup-db:
    action: init-database
  # checkout and setup-db run in parallel
  build:
    action: npm-build
    depends_on: [checkout]
  test:
    action: run-tests
    depends_on: [checkout, setup-db]
    # test waits for both checkout AND setup-db
```

### Passing data between steps

When a step emits structured output (via `OUTPUT: {json}`), downstream steps can reference it in templates.

The template variable name uses the **step name with hyphens replaced by underscores**, because the Tera template engine interprets hyphens as subtraction.

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
          # say-hello -> say_hello (hyphens become underscores)
          message: "{{ say_hello.output.greeting }}"
```

**Important**: Step name `say-hello` becomes `say_hello` in templates. Always use underscores when referencing step outputs.

## Templating

Strøm uses [Tera](https://keats.github.io/tera/) for templating. Templates are rendered when a worker claims a step.

### Available context

Inside a step's `input` templates, you have access to:

| Variable | Description |
|----------|-------------|
| `input.*` | Job-level input (from the API call or trigger) |
| `<step_name>.output.*` | Output from a completed upstream step |

### Tera features

Tera supports filters, conditionals, and more:

```yaml
# Filters
cmd: "echo {{ name | upper }}"
cmd: "echo {{ name | default(value='World') }}"

# Conditionals
cmd: "{% if enabled %}echo Active{% else %}echo Inactive{% endif %}"
```

### Step name rules

- Step names in YAML can use hyphens: `say-hello`
- In template references, use underscores: `{{ say_hello.output.* }}`
- This is because Tera treats `-` as the subtraction operator

## Organizing Tasks with Folders

Tasks can be organized into folders using the optional `folder` property. The UI displays tasks in a collapsible folder tree when folders are present.

### Basic folder

```yaml
tasks:
  deploy-staging:
    folder: deploy
    flow:
      run:
        action: deploy-app
```

### Nested folders

Use `/` to create nested folder hierarchies:

```yaml
tasks:
  deploy-staging:
    folder: deploy/staging
    flow:
      run:
        action: deploy-app

  deploy-production:
    folder: deploy/production
    flow:
      run:
        action: deploy-app

  run-etl:
    folder: data/pipelines
    flow:
      extract:
        action: extract-data
```

This creates a tree structure in the UI:

```
deploy/
  staging/
    deploy-staging
  production/
    deploy-production
data/
  pipelines/
    run-etl
```

Tasks without a `folder` property appear at the root level. When no tasks have folders, the UI shows a flat table as before.

## Input Parameters

Both actions and tasks can declare input parameters.

```yaml
input:
  name:
    type: string          # parameter type (string for now)
    required: true         # fails if not provided
  env:
    type: string
    default: "staging"     # used when not provided
```

When triggering a task via API:

```bash
curl -X POST http://localhost:8080/api/workspaces/default/tasks/deploy-pipeline/execute \
  -H "Content-Type: application/json" \
  -d '{"input": {"env": "production"}}'
```

## Triggers

Triggers define automated task execution. Currently supported: `scheduler` (cron-based).

### Cron scheduler

```yaml
triggers:
  nightly-backup:
    type: scheduler
    cron: "0 0 2 * * *"
    task: backup-db
    input:
      retention_days: 30
    enabled: true
```

The `cron` field supports both standard 5-field (minute granularity) and extended 6-field (second granularity) expressions:

```
# 5-field: minute hour day-of-month month day-of-week
cron: "0 2 * * *"           # Every day at 2:00 AM

# 6-field: second minute hour day-of-month month day-of-week
cron: "0 0 2 * * *"         # Every day at 2:00:00 AM
cron: "*/10 * * * * *"      # Every 10 seconds
cron: "0 30 9 * * MON-FRI"  # Weekdays at 9:30 AM
```

Extended cron features (via the `croner` library):
- `L` — last day of month (`L` in day-of-month) or last weekday occurrence (`5L` = last Friday)
- `#` — nth weekday (`5#2` = second Friday of the month)
- `W` — closest weekday to a day (`15W` = closest weekday to the 15th)
- Text names — `MON`, `TUE`, `JAN`, `FEB`, etc.

### Trigger fields

| Field | Required | Description |
|-------|----------|-------------|
| `type` | Yes | Trigger type: `scheduler` or `webhook` |
| `cron` | For scheduler | Cron expression (5 or 6 fields) |
| `task` | Yes | Name of the task to execute |
| `input` | No | Input values passed to the task |
| `enabled` | No | Whether the trigger is active (default: `true`) |

### How the scheduler works

- The scheduler runs inside the server process and wakes only when a trigger is due (no fixed polling interval).
- When workspace configs are hot-reloaded (every 30 seconds), the scheduler picks up new/changed/removed triggers automatically.
- If a trigger's cron expression changes, its next fire time is recalculated. If unchanged, the existing schedule is preserved.
- Jobs created by triggers have `source_type: "trigger"` and `source_id: "{workspace}/{trigger_name}"` for audit trail.
- If the server was down when a trigger was due, it fires on the next startup.

## Validation

Use the CLI to validate workflow files before deploying:

```bash
# Validate a single file
stroem validate workspace/.workflows/deploy.yaml

# Validate all files in a directory
stroem validate workspace/.workflows/
```

The validator checks:
- YAML syntax and structure
- Action type validity (`shell` needs `cmd` or `script`; `docker`/`pod` need `image`; `task` needs `task`)
- `shell` actions cannot have `image` (use `runner: docker` instead)
- `docker`/`pod` actions cannot have `runner` or `script`
- `task` actions cannot have `cmd`, `script`, `image`, or `runner`
- `task` actions must reference an existing task in the same workspace
- Self-referencing task actions (task A → action → task A) are rejected
- `runner` values are valid (`local`, `docker`, `pod`)
- Flow steps reference existing actions
- Dependencies reference existing steps within the same flow
- No cycles in the dependency graph
- Trigger cron expressions are valid (catches syntax errors before deployment)
- Scheduler triggers have a `cron` field

## Multi-Workspace Setup

Strøm supports multiple workspaces, each with its own set of workflow files. Configure workspaces in `server-config.yaml`:

```yaml
workspaces:
  default:
    type: folder
    path: ./workspace
  data-team:
    type: git
    url: https://github.com/org/data-workflows.git
    ref: main
    poll_interval_secs: 60
```

Each workspace is independent -- tasks, actions, and scripts are scoped to their workspace. Tasks are accessed via workspace-scoped API routes:

```bash
# List tasks in a specific workspace
curl http://localhost:8080/api/workspaces/data-team/tasks

# Trigger a task in a specific workspace
curl -X POST http://localhost:8080/api/workspaces/data-team/tasks/etl-pipeline/execute \
  -H "Content-Type: application/json" \
  -d '{"input": {"date": "2025-01-01"}}'
```

Workers automatically download the correct workspace files before executing each step.

## Complete Example

A deploy pipeline with health check, deployment, and notification:

```yaml
# workspace/.workflows/deploy.yaml
actions:
  check-status:
    type: shell
    cmd: "echo 'Checking system status...' && sleep 1 && echo 'OUTPUT: {\"healthy\": true}'"

  deploy:
    type: shell
    script: actions/deploy.sh
    input:
      env: { type: string, default: "staging" }

  notify:
    type: shell
    cmd: "echo 'Notification: Deployment to {{ input.env }} completed with status={{ input.status }}'"
    input:
      env: { type: string }
      status: { type: string }

tasks:
  deploy-pipeline:
    mode: distributed
    input:
      env: { type: string, default: "staging" }
    flow:
      health-check:
        action: check-status
      deploy-app:
        action: deploy
        depends_on: [health-check]
        input:
          env: "{{ input.env }}"
      send-notification:
        action: notify
        depends_on: [deploy-app]
        input:
          env: "{{ input.env }}"
          status: "success"
```

The corresponding script:

```bash
# workspace/actions/deploy.sh
#!/bin/bash
set -euo pipefail

echo "Deploying to environment: ${DEPLOY_ENV:-staging}"
echo "Version: ${VERSION:-latest}"
sleep 1
echo "Deployment complete!"
echo "OUTPUT: {\"status\": \"deployed\", \"env\": \"${DEPLOY_ENV:-staging}\"}"
```

---

## Hooks: on_success and on_error

Tasks can define `on_success` and `on_error` hooks that fire automatically when a job reaches a terminal state. Each hook references an existing action and can pass input rendered with job context.

### Syntax

```yaml
tasks:
  deploy:
    flow:
      build:
        action: build-app
      deploy:
        action: deploy-app
        depends_on: [build]
    on_success:
      - action: notify-slack
        input:
          message: "Deploy {{ hook.task_name }} succeeded ({{ hook.duration_secs }}s)"
    on_error:
      - action: notify-slack
        input:
          message: "Deploy {{ hook.task_name }} FAILED: {{ hook.error_message }}"
```

### Hook Context Variables

Hook input values are Tera templates with access to a `hook` object containing job context:

| Variable | Type | Description |
|----------|------|-------------|
| `hook.workspace` | string | Workspace name |
| `hook.task_name` | string | Task that completed/failed |
| `hook.job_id` | string | UUID of the original job |
| `hook.status` | string | `"completed"` or `"failed"` |
| `hook.is_success` | bool | true if completed |
| `hook.error_message` | string/null | All failed step errors combined |
| `hook.source_type` | string | Original job source (`"api"`, `"trigger"`, etc.) |
| `hook.source_id` | string/null | Original job source ID |
| `hook.started_at` | string/null | ISO 8601 timestamp |
| `hook.completed_at` | string/null | ISO 8601 timestamp |
| `hook.duration_secs` | number/null | Execution duration in seconds |
| `hook.failed_steps` | array | Failed step details (see below) |

Each entry in `hook.failed_steps` contains:

| Field | Type | Description |
|-------|------|-------------|
| `step_name` | string | Name of the failed step |
| `action_name` | string | Action that was executed |
| `error_message` | string/null | The step's error message |
| `continue_on_failure` | bool | Whether the step had `continue_on_failure` set |

### Behavior

- **Fire-and-forget**: Hook job creation is best-effort. Failures are logged but never affect the original job.
- **No recursion**: Jobs created by hooks (`source_type = "hook"`) never trigger further hooks.
- **Visible as jobs**: Hook jobs appear in the job list with `task_name = "_hook:<action>"` and `source_type = "hook"`.
- **Normal execution**: Hook jobs go through the normal claim/execute flow on workers.
- **Multiple hooks**: You can define multiple hooks per event. They all fire independently.
- **Validation**: Hook action references are validated at parse time — referencing a non-existent action is an error.
