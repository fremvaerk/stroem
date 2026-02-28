---
title: Workflow Basics
description: YAML structure, tasks, steps, and DAG execution
---

Workflows in Strøm are defined as YAML files containing **actions** and **tasks**.

## File location

Workflow files go in the `.workflows/` directory of each workspace and must have a `.yaml` or `.yml` extension. The server loads all files from this directory on startup.

A single YAML file can contain multiple actions and tasks.

For the default folder workspace, files are at `workspace/.workflows/`. For git-sourced workspaces, the repository must contain a `.workflows/` directory at the root.

## YAML structure

```yaml
actions:
  <action-name>:
    name: Human-Readable Name    # optional display name
    description: What it does    # optional description
    type: shell
    cmd: "..."
    input:
      <param-name>:
        type: string
        description: What this parameter is for  # optional
        required: true
      <param-name>: { type: string, default: "value" }

tasks:
  <task-name>:
    name: Human-Readable Name    # optional display name
    description: What this task does  # optional description
    mode: distributed
    folder: <optional-folder-path>
    input:
      <param-name>: { type: string, default: "value" }
    flow:
      # Reference a named action:
      <step-name>:
        action: <action-name>
        name: Human-Readable Step Name  # optional display name
        description: What this step does  # optional description
        depends_on: [<other-step>]
        input:
          <param>: "{{ input.param }}"
      # Or define the action inline:
      <step-name>:
        type: shell
        cmd: "..."
        name: Human-Readable Step Name  # optional
        depends_on: [<other-step>]
```

## Actions

Actions are the smallest execution unit. Each action defines a command or script that runs on a worker. See [Action Types](/guides/action-types/) for all supported types.

Actions support optional `name` and `description` fields for human-readable labeling:

```yaml
actions:
  greet:
    name: Greet User
    description: Sends a greeting message to the specified user
    type: shell
    cmd: "echo Hello {{ input.name }}"
    input:
      name:
        type: string
        description: The user's name
        required: true
```

## Tasks

Tasks compose actions into a DAG (directed acyclic graph) of steps.

### Basic task

Tasks support optional `name` and `description` fields, and flow steps can also have their own `name` and `description`:

```yaml
tasks:
  hello-world:
    name: Hello World
    description: A simple greeting task
    mode: distributed
    input:
      name: { type: string, default: "World" }
    flow:
      say-hello:
        action: greet
        name: Say Hello
        description: Greet the user by name
        input:
          name: "{{ input.name }}"
```

The `name` and `description` are displayed in the web UI. When omitted, the YAML key (e.g. `hello-world`, `say-hello`) is used as the display label.

### Inline actions

For simple, one-off steps you can define the action inline instead of referencing a named action:

```yaml
tasks:
  hello:
    flow:
      say-hi:
        type: shell
        cmd: "echo Hello, World!"
```

This is equivalent to defining a separate action and referencing it:

```yaml
actions:
  greet:
    type: shell
    cmd: "echo Hello, World!"

tasks:
  hello:
    flow:
      say-hi:
        action: greet
```

Inline steps support all action fields (`type`, `cmd`, `script`, `image`, `runner`, `env`, `tags`, etc.) plus all step fields (`depends_on`, `input`, `continue_on_failure`):

```yaml
tasks:
  deploy:
    input:
      env: { type: string, default: "staging" }
    flow:
      build:
        type: docker
        image: node:20
        command: ["npm", "run", "build"]
      deploy:
        type: shell
        runner: docker
        cmd: "deploy.sh {{ input.env }}"
        depends_on: [build]
        input:
          env: "{{ input.env }}"
      notify:
        type: shell
        cmd: "echo Done"
        depends_on: [deploy]
        continue_on_failure: true
```

Use inline actions for steps that are unique to a single task. Use named actions when the same action is shared across multiple tasks or steps.

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

This creates a linear pipeline: `health-check` → `deploy-app` → `send-notification`.

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

The `continue_on_failure` flag has dual semantics (similar to GitHub Actions' `continue-on-error`):

1. **Dependency tolerance**: The step runs even if its dependencies fail.
2. **Job tolerance**: If the step itself fails, its failure is considered *tolerable* — the job can still be marked `completed` as long as all non-tolerable steps succeed.

### DAG visualization

The web UI provides an interactive graph view for step dependencies:

- **Job Detail page**: Toggle between "Timeline" and "Graph" views. The graph shows each step as a node with live status (color-coded borders, animated edges for running steps). Click a node to view step details.
- **Task Detail page**: Tasks with more than one step display a dependency graph above the step list.

### Organizing tasks with folders

Tasks can be organized into folders using the optional `folder` property. The UI displays tasks in a collapsible folder tree when folders are present.

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
```

Use `/` to create nested folder hierarchies. Tasks without a `folder` property appear at the root level.

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
- Runner field validity (`local`, `docker`, `pod`)
- Flow steps reference existing actions
- Dependencies reference existing steps within the same flow
- No cycles in the dependency graph
- Trigger cron expressions are valid
- Hook action references exist
