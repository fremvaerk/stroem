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

Scripts are relative to the workspace root.

```yaml
actions:
  deploy:
    type: shell
    script: actions/deploy.sh
    input:
      env: { type: string, default: "staging" }
```

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

In this example, `notify` runs regardless of whether `deploy` succeeds or fails. The job is still marked `failed` if any step failed.

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
- Action type validity (shell actions need `cmd` or `script`)
- Flow steps reference existing actions
- Dependencies reference existing steps within the same flow
- No cycles in the dependency graph

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
