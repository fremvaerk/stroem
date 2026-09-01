---
title: Templating
description: Tera template engine, variables, and step name rules
---

Strøm uses [Tera](https://keats.github.io/tera/) for templating. Templates are rendered when a worker claims a step (or server-side for task actions).

## Available context

Inside a step's `input` templates and `when` conditions, you have access to:

| Variable | Description |
|----------|-------------|
| `input.*` | Job-level input (from the API call or trigger) |
| `<step_name>.output.*` | Output from a completed upstream step |
| `secret.*` | Resolved workspace secrets |
| `job.revision` | Workspace revision (git SHA or folder hash) pinned on the job at creation |
| `state.*` | Previous [task state snapshot](/guides/task-state/) |
| `global_state.*` | Previous [global workspace state](/guides/task-state/#global-workspace-state) |
| `each.*` | Loop variables inside [`for_each` steps](/guides/loops/) |

## Basic usage

```yaml
actions:
  greet:
    type: script
    script: "echo Hello {{ input.name }}"
    input:
      name: { type: string, required: true }
```

## Passing data between steps

When a step emits structured output (via `OUTPUT: {json}`), downstream steps can reference it in templates.

```yaml
actions:
  greet:
    type: script
    script: "echo Hello {{ input.name }} && echo 'OUTPUT: {\"greeting\": \"Hello {{ input.name }}\"}'"
    input:
      name: { type: string, required: true }

  shout:
    type: script
    script: "echo {{ input.message }} | tr '[:lower:]' '[:upper:]'"
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

## Job metadata

Every job pins the workspace revision (git commit SHA for git workspaces, content hash for folder workspaces) at creation time. It is available as `{{ job.revision }}` in step inputs, `when` conditions, action bodies (`script`, `cmd`, `env`, `args`, `image`, `manifest`), agent prompts, and approval messages:

```yaml
actions:
  deploy:
    type: docker
    # Deploy the image built from the exact commit this job runs at
    image: "my-registry/app:{{ job.revision }}"

  report:
    type: script
    script: "echo Deployed revision {{ job.revision }}"
```

The value is identical for every step of a job (sub-jobs and hook jobs inherit the parent's revision). For jobs created before revision tracking existed it renders as an empty string. In hooks, the same value is available as `hook.revision` — see [Hooks](/guides/hooks/).

:::note
A flow step literally named `job` shadows the job metadata: `{{ job.output.* }}` keeps referring to that step's output, and `{{ job.revision }}` is unavailable in that task. Avoid naming a step `job` if you want the metadata.
:::

## Step name rules

:::caution
Step names with hyphens (e.g., `say-hello`) are sanitized to underscores (`say_hello`) in the template context because Tera interprets hyphens as subtraction.
:::

- Step names in YAML can use hyphens: `say-hello`
- In template references, use underscores: `{{ say_hello.output.* }}`

## Tera features

Tera supports filters, conditionals, and more:

```yaml
# Filters
script: "echo {{ name | upper }}"
script: "echo {{ name | default(value='World') }}"

# Conditionals
script: "{% if enabled %}echo Active{% else %}echo Inactive{% endif %}"
```

See the [Tera documentation](https://keats.github.io/tera/docs/) for the full feature set.

## Input defaults with templates

Task input defaults support Tera templates with access to `secret.*`. Defaults are rendered at job creation time, before the job is persisted:

```yaml
tasks:
  deploy:
    input:
      api_key:
        type: string
        default: "{{ secret.DEPLOY_KEY }}"
```

See [Input & Output](/guides/input-and-output/) for full details on default values.

## Secret references in templates

The `| vals` filter resolves secret references at template render time. See [Secrets & Encryption](/guides/secrets/) for details.

```yaml
env:
  DB_PASSWORD: "{{ 'ref+awsssm:///prod/db/password' | vals }}"
```

## Conditional step execution

Steps support a `when` field for conditional execution. The condition is a Tera template that evaluates to true or false when the step's dependencies are met:

```yaml
tasks:
  conditional:
    input:
      run_checks: { type: boolean, default: false }
    flow:
      check:
        action: validate
        when: "{{ input.run_checks }}"

      process:
        action: process-data
        depends_on: [check]
        continue_on_failure: true
        # Runs whether check ran or was skipped
```

See [Conditional Flow Steps](/guides/conditionals/) for the full feature documentation.
