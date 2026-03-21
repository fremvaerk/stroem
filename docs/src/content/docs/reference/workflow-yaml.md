---
title: Workflow YAML
description: Complete reference for all workflow YAML fields
---

Workflow files live in `.workflows/` and contain `actions`, `tasks`, `triggers`, `connections`, and `secrets`. This page documents every available field.

## Top-Level Structure

```yaml
secrets:
  MY_SECRET: "{{ env.MY_SECRET }}"

connection_types:
  <name>: { ... }

connections:
  <name>: { ... }

actions:
  <name>: { ... }

tasks:
  <name>: { ... }

triggers:
  <name>: { ... }

on_success:
  - action: <name>
on_error:
  - action: <name>
```

All top-level keys are optional. A file typically defines `actions` and `tasks`.

The top-level `on_success` and `on_error` hooks act as workspace-level fallbacks — they fire for top-level jobs when the task itself has no hooks defined for that event type.

---

## Actions

Actions are the smallest execution unit. Each action defines what runs on a worker.

### Common fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | **required** | Action type: `script`, `docker`, `pod`, `task`, `agent`, or `approval` |
| `name` | string | — | Human-readable display name |
| `description` | string | — | What this action does |
| `input` | map | `{}` | Input parameter schema (see [Input fields](#input-fields)) |
| `output` | object | — | Output schema (see [Output fields](#output-fields)) |
| `env` | map | — | Environment variables. Values support Tera templates |
| `tags` | list | `[]` | Worker routing tags for step claiming |
| `workdir` | string | — | Working directory override |
| `resources` | object | — | CPU/memory requests (see [Resources](#resources)) |

### `type: script`

Runs a script in a [runner](/guides/runners/) environment with workspace files mounted.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `script` | string | — | Inline script content. Mutually exclusive with `source` |
| `source` | string | — | Path to script file (relative to workspace root). Mutually exclusive with `script` |
| `runner` | string | `local` | Execution environment: `local`, `docker`, or `pod` |
| `language` | string | `shell` | Script language: `shell`, `python`, `javascript` (or `js`), `typescript` (or `ts`), `go` |
| `dependencies` | list | `[]` | Packages to install before running. Requires non-shell language |
| `interpreter` | string | — | Override auto-detected interpreter binary (e.g., `python3.11`) |
| `manifest` | object | — | K8s pod manifest overrides. Only valid with `runner: pod` |

Either `script` or `source` is required. Cannot use `image`, `cmd`, or `entrypoint`.

**Toolchain preferences** (auto-detected when `interpreter` is not set):
- Python: `uv` > `python3` > `python`
- JavaScript: `bun` > `node`
- TypeScript: `bun` > `deno`
- Shell: `bash` > `sh`

```yaml
actions:
  run-tests:
    type: script
    language: python
    dependencies: [pytest, requests]
    script: |
      import pytest
      pytest.main(["-v", "tests/"])
```

### `type: docker`

Runs a container image as-is, without workspace mounting.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `image` | string | **required** | Docker image URI |
| `cmd` | string | — | Shell command to run in the container |
| `entrypoint` | list | — | Container entrypoint override |
| `command` | list | — | Container command args |

Cannot use `runner`, `script`, `source`, `language`, `dependencies`, or `interpreter`.

```yaml
actions:
  run-migration:
    type: docker
    image: myapp/migrations:latest
    command: ["migrate", "--target", "latest"]
```

### `type: pod`

Runs a Kubernetes pod. Same fields as `type: docker`, plus:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `manifest` | object | — | Raw JSON/YAML deep-merged into the generated pod spec |

The `manifest` field supports service accounts, node selectors, tolerations, resource limits, annotations, sidecars, and other pod-level configuration.

```yaml
actions:
  gpu-inference:
    type: pod
    image: myapp/inference:latest
    manifest:
      spec:
        containers:
          - name: main
            resources:
              limits:
                nvidia.com/gpu: "1"
        tolerations:
          - key: nvidia.com/gpu
            operator: Exists
            effect: NoSchedule
```

### `type: task`

References another task, creating a child job. The step is dispatched server-side (workers never claim it).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `task` | string | **required** | Name of the task to execute |

Cannot use any other execution fields (`cmd`, `script`, `source`, `image`, `runner`, `language`, `manifest`, etc.).

```yaml
actions:
  run-deploy:
    type: task
    task: deploy-pipeline

tasks:
  orchestrate:
    flow:
      deploy:
        action: run-deploy
        input:
          env: "production"

  deploy-pipeline:
    input:
      env: { type: string }
    flow:
      # ... steps
```

Child jobs have a max nesting depth of 10 levels.

### `type: agent`

Calls an LLM as a workflow step. The step is dispatched server-side (workers never claim it).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `provider` | string | **required** | Provider ID from server config |
| `prompt` | string | **required** | Tera template for the user message |
| `system_prompt` | string | — | Tera template for system/instruction message |
| `model` | string | — | Override provider's default model |
| `max_tokens` | integer | — | Override provider's max tokens |
| `temperature` | number | — | Override provider's temperature (0–2) |
| `output` | object | — | Output schema (converted to JSON Schema at dispatch) |

Cannot use any other execution fields (`cmd`, `script`, `source`, `image`, `runner`, `language`, `manifest`, `task`, etc.).

```yaml
actions:
  classify-ticket:
    type: agent
    provider: anthropic-main
    system_prompt: "You are a support ticket classifier."
    prompt: "Classify this ticket: {{ input.ticket_body }}"
    output:
      properties:
        category:
          type: string
          required: true
          options: [bug, feature_request, question]
        confidence:
          type: number

tasks:
  handle-support:
    input:
      ticket_body: { type: string }
    flow:
      classify:
        action: classify-ticket
        input:
          ticket_body: "{{ input.ticket_body }}"
      route-bug:
        action: escalate
        depends_on: [classify]
        when: "{{ classify.output.category == 'bug' }}"
```

### `type: approval`

Pauses execution for human approval/rejection. The step is dispatched server-side (workers never claim it).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `message` | string | — | Tera template for the approval message shown to approver |

Cannot use any other execution fields (`cmd`, `script`, `source`, `image`, `runner`, `language`, `manifest`, `task`, `provider`, `prompt`, etc.).

```yaml
actions:
  approve-deployment:
    type: approval
    message: |
      Deploy to production?
      Version: {{ input.version }}
      Environment: {{ input.env }}

tasks:
  deploy-workflow:
    input:
      version: { type: string }
      env: { type: string }
    flow:
      manual-approval:
        action: approve-deployment
        input:
          version: "{{ input.version }}"
          env: "{{ input.env }}"
      deploy:
        action: run-deployment
        depends_on: [manual-approval]
        input:
          version: "{{ input.version }}"
```

When approved, the step's output is `{ "approved": true, "approved_by": "user@example.com", "approved_at": "2025-03-21T10:30:00Z", "input": {...} }`. When rejected, the step fails and downstream steps are skipped.

---

## Input Fields

Input parameters define the schema for action and task inputs.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | **required** | `string`, `text`, `integer`, `number`, `boolean`, `date`, `datetime`, or a connection type name |
| `name` | string | — | Human-readable label for the UI |
| `description` | string | — | Help text |
| `required` | bool | `false` | Whether the field must be provided |
| `secret` | bool | `false` | Mask value in UI and logs |
| `default` | any | — | Default value when not provided |
| `options` | list | — | Predefined dropdown choices |
| `allow_custom` | bool | `false` | Allow values outside `options` list |
| `order` | integer | — | Display order in UI (lower = earlier). Fields without `order` appear last |

```yaml
input:
  environment:
    type: string
    description: Target environment
    required: true
    options: [staging, production]
  version:
    type: string
    default: "latest"
  dry_run:
    type: boolean
    default: false
    order: 99
```

When `type` is not a primitive, it references a [connection type](#connection-types). The input value (a connection name) is resolved to the full connection object at execution time.

---

## Output Fields

Output schemas describe the structured output of an action. Script actions emit output by printing `OUTPUT: {json}` to stdout. Agent actions use `output` to instruct the LLM to respond with structured JSON.

```yaml
actions:
  build:
    type: script
    script: |
      echo "Building..."
      echo 'OUTPUT: {"artifact": "dist/app.tar.gz", "size": 1024}'
    output:
      properties:
        artifact: { type: string }
        size: { type: integer }
```

| Field | Type | Description |
|-------|------|-------------|
| `output.properties` | map | Map of field names to output field definitions |

**Output field properties:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | String | Required | `string`, `integer`, `number`, `boolean`, `array`, `object` |
| `description` | String | — | Human-readable description |
| `required` | Boolean | `false` | Whether the field must be present |
| `default` | Any | — | Default value |
| `options` | Array | — | Allowed values (maps to JSON Schema `enum`) |

For agent actions, `output` is converted to JSON Schema at dispatch time and injected into the system prompt.

---

## Resources

CPU and memory requests for runner containers.

| Field | Type | Description |
|-------|------|-------------|
| `cpu` | string | CPU request (e.g., `"500m"`, `"1"`) |
| `memory` | string | Memory request (e.g., `"256Mi"`, `"1Gi"`) |

```yaml
actions:
  heavy-compute:
    type: script
    runner: pod
    resources:
      cpu: "2"
      memory: "4Gi"
    script: "..."
```

---

## Tasks

Tasks compose actions into a DAG of steps.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | — | Human-readable display name |
| `description` | string | — | What this task does |
| `mode` | string | `distributed` | Execution mode: `distributed` |
| `folder` | string | — | UI folder path (e.g., `deploy/staging`). Use `/` for nesting |
| `input` | map | `{}` | Input parameter schema (see [Input fields](#input-fields)) |
| `flow` | map | **required** | Steps — map of step name to [flow step](#flow-steps) |
| `timeout` | duration | — | Job-level timeout. Max `7d` (604800s). Cancels entire job when exceeded |
| `on_success` | list | `[]` | [Hooks](#hooks) to run when the job succeeds |
| `on_error` | list | `[]` | [Hooks](#hooks) to run when the job fails |

```yaml
tasks:
  deploy-pipeline:
    name: Deploy Pipeline
    description: Build, deploy, and verify
    folder: deploy
    timeout: 30m
    input:
      env: { type: string, default: "staging" }
    flow:
      build:
        action: build-app
      deploy:
        action: deploy
        depends_on: [build]
    on_error:
      - action: notify
        input:
          message: "Deploy failed: {{ hook.error_message }}"
```

---

## Flow Steps

Each entry in a task's `flow` map defines a step. Steps can reference a named action or define one inline.

### Step fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `action` | string | **required** | Action name to execute. Omit when using inline action |
| `name` | string | — | Human-readable step display name |
| `description` | string | — | What this step does |
| `depends_on` | list | `[]` | Steps that must complete before this one starts |
| `input` | map | `{}` | Input values passed to the action. Values support Tera templates |
| `continue_on_failure` | bool | `false` | Run even if dependencies fail; mark own failure as tolerable |
| `timeout` | duration | — | Step execution timeout. Max `24h` (86400s) |
| `when` | string | — | Tera condition. Falsy values: empty string, `"false"`, `"0"`, `"null"`, `"none"` (case-insensitive) |
| `for_each` | string or list | — | Tera expression or literal JSON array. Creates one instance per item |
| `sequential` | bool | `false` | Run `for_each` instances one at a time instead of in parallel |

### Inline actions

Instead of referencing a named action, define one inline by adding action fields directly on the step:

```yaml
flow:
  greet:
    type: script
    script: "echo Hello {{ input.name }}"
    input:
      name: "{{ input.name }}"
```

All [action fields](#actions) are valid on inline steps (e.g., `type`, `script`, `source`, `image`, `runner`, `language`, `env`, `tags`).

### Dependencies

```yaml
flow:
  a:
    action: step-a
  b:
    action: step-b
  c:
    action: step-c
    depends_on: [a, b]    # waits for both a and b
```

Steps without `depends_on` start immediately. Failed dependencies cause downstream steps to be skipped unless `continue_on_failure: true`.

Skipped dependencies (from `when` conditions) are treated as satisfied — downstream steps still run as long as at least one dependency completed.

### Conditional steps (`when`)

```yaml
flow:
  deploy:
    action: deploy-app
    when: "{{ input.env == 'production' }}"
    input:
      env: "{{ input.env }}"
```

Evaluated before `for_each`. If the condition errors, the step fails (not silently skipped). See the [Conditionals guide](/guides/conditionals/) for details.

### For-each loops

```yaml
flow:
  deploy:
    action: deploy-to-region
    for_each: ["us-east-1", "eu-west-1", "ap-south-1"]
    input:
      region: "{{ each.item }}"
      index: "{{ each.index }}"
```

Creates `deploy[0]`, `deploy[1]`, `deploy[2]`. Inside instances, `each.item` is the current value and `each.index` is the zero-based position.

Downstream steps receive the aggregated output as an array via `{{ step_name.output }}`.

**Limits**: max 10,000 items. Step names must not contain `[` or `]`. See the [Loops guide](/guides/loops/) for sequential mode, error handling, and sub-job fan-out.

### Timeouts

Accepts human-readable durations: `30s`, `5m`, `1h30m`, or a plain integer (seconds).

```yaml
flow:
  build:
    action: build-app
    timeout: 10m
```

Enforced both server-side (recovery sweeper) and worker-side (process cancellation).

---

## Hooks

Hooks fire when a job reaches a terminal state. Defined on tasks or at the top level of the workflow file.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `action` | string | **required** | Action to execute. Can be a `type: task` action for full child job |
| `input` | map | `{}` | Input values. Supports Tera templates with `hook` context |

### Hook template context

| Variable | Type | Description |
|----------|------|-------------|
| `hook.workspace` | string | Workspace name |
| `hook.task_name` | string | Task that completed/failed |
| `hook.job_id` | string | Job UUID |
| `hook.status` | string | `"completed"` or `"failed"` |
| `hook.is_success` | bool | `true` if completed |
| `hook.error_message` | string/null | All failed step errors combined |
| `hook.source_type` | string | Original job source (`"api"`, `"trigger"`, etc.) |
| `hook.source_id` | string/null | Original job source ID |
| `hook.started_at` | string/null | ISO 8601 timestamp |
| `hook.completed_at` | string/null | ISO 8601 timestamp |
| `hook.duration_secs` | number/null | Execution duration in seconds |
| `hook.failed_steps` | list | Failed step details (see below) |

Each entry in `hook.failed_steps`:

| Field | Type | Description |
|-------|------|-------------|
| `step_name` | string | Name of the failed step |
| `action_name` | string | Action that was executed |
| `error_message` | string/null | The step's error message |
| `continue_on_failure` | bool | Whether the step had `continue_on_failure` set |

```yaml
tasks:
  deploy:
    flow:
      # ...
    on_success:
      - action: notify
        input:
          message: "Deploy completed in {{ hook.duration_secs }}s"
    on_error:
      - action: notify
        input:
          message: "Deploy failed: {{ hook.error_message }}"
```

Hook jobs use `source_type = "hook"` and never trigger further hooks (recursion guard).

---

## Triggers

### Scheduler (`type: scheduler`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | **required** | `scheduler` |
| `cron` | string | **required** | Cron expression (5-field, or 6-field with optional seconds) |
| `task` | string | **required** | Task to execute |
| `input` | map | `{}` | Input values |
| `enabled` | bool | `true` | Whether the trigger is active |
| `timezone` | string | UTC | IANA timezone name (e.g., `Europe/Copenhagen`) |
| `concurrency` | string | `allow` | `allow`, `skip`, or `cancel_previous` |

```yaml
triggers:
  nightly-backup:
    type: scheduler
    cron: "0 2 * * *"
    task: run-backup
    timezone: "Europe/Copenhagen"
    concurrency: skip
```

### Webhook (`type: webhook`)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | **required** | `webhook` |
| `name` | string | **required** | URL-safe name (alphanumeric, `-`, `_`). Endpoint: `/hooks/{name}` |
| `task` | string | **required** | Task to execute |
| `secret` | string | — | Auth via `?secret=xxx` or `Authorization: Bearer xxx` |
| `input` | map | `{}` | Default input values. Request body/headers/query merged in |
| `enabled` | bool | `true` | Whether the webhook is active |
| `mode` | string | `async` | `async` (fire-and-forget) or `sync` (wait for job completion) |
| `timeout_secs` | integer | `30` | Max wait in sync mode (1–300). Returns 202 on timeout |

```yaml
triggers:
  github-push:
    type: webhook
    name: github-ci
    task: ci-pipeline
    secret: "whsec_your_secret"
    mode: sync
    timeout_secs: 120
```

---

## Connection Types

Define schemas for reusable connection configurations.

| Field | Type | Description |
|-------|------|-------------|
| `properties` | map | Map of property names to property definitions |

### Property fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | **required** | `string`, `text`, `integer`, `number`, `boolean`, `date`, or `datetime` |
| `required` | bool | `false` | Whether the property must be provided |
| `default` | any | — | Default value |
| `secret` | bool | `false` | Mask in UI and logs |

```yaml
connection_types:
  postgres:
    properties:
      host: { type: string, required: true }
      port: { type: integer, default: 5432 }
      database: { type: string, required: true }
      username: { type: string, required: true }
      password: { type: string, required: true, secret: true }
```

## Connections

Named instances of connection types.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `type` | string | — | Connection type reference (for validation). Optional for untyped connections |
| *(other keys)* | any | — | Property values (flattened alongside `type`) |

```yaml
connections:
  prod-db:
    type: postgres
    host: "db.example.com"
    port: 5432
    database: myapp
    username: "{{ secrets.DB_USER }}"
    password: "{{ secrets.DB_PASS }}"
```

Use connection names as input values when the input field's `type` references a connection type. The name is resolved to the full connection object at execution time.

---

## Secrets

Key-value map rendered through Tera at workspace load time. Typically sources values from environment variables:

```yaml
secrets:
  DB_PASSWORD: "{{ env.DB_PASSWORD }}"
  API_KEY: "{{ env.MY_API_KEY }}"
  STATIC_VALUE: "hardcoded-is-fine-too"
```

Secrets are available in connection values and action templates via `{{ secrets.DB_PASSWORD }}`.

---

## Duration Format

Timeout fields accept human-readable durations or plain integers (seconds):

| Format | Example | Seconds |
|--------|---------|---------|
| Seconds | `30s` | 30 |
| Minutes | `5m` | 300 |
| Hours | `1h` | 3600 |
| Combined | `1h30m` | 5400 |
| Plain integer | `300` | 300 |
