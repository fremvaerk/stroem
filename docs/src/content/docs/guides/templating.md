---
title: Templating
description: Tera template engine, variables, and step name rules
---

Strøm uses [Tera](https://keats.github.io/tera/) for templating. Templates are rendered when a worker claims a step (or server-side for task actions).

## Available context

Inside a step's `input` templates, you have access to:

| Variable | Description |
|----------|-------------|
| `input.*` | Job-level input (from the API call or trigger) |
| `<step_name>.output.*` | Output from a completed upstream step |
| `secret.*` | Resolved workspace secrets |

## Basic usage

```yaml
actions:
  greet:
    type: shell
    cmd: "echo Hello {{ input.name }}"
    input:
      name: { type: string, required: true }
```

## Passing data between steps

When a step emits structured output (via `OUTPUT: {json}`), downstream steps can reference it in templates.

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
cmd: "echo {{ name | upper }}"
cmd: "echo {{ name | default(value='World') }}"

# Conditionals
cmd: "{% if enabled %}echo Active{% else %}echo Inactive{% endif %}"
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
