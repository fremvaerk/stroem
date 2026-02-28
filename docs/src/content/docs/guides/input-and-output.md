---
title: Input & Output
description: Input parameters, structured output, and organizing tasks with folders
---

## Input parameters

Both actions and tasks can declare input parameters.

```yaml
input:
  user_name:
    type: string          # single-line text
    name: User name       # optional, human-readable label in UI
    description: The user's full name  # optional, shown as helper text
    required: true         # fails if not provided
  env:
    type: string
    name: Environment
    description: Target deployment environment
    default: "staging"     # used when not provided
  query:
    type: text            # multiline text (textarea in UI)
    description: SQL query to execute
```

The optional `name` field provides a human-readable label for the input field in the web UI. When not set, the YAML key (e.g. `user_name`) is used as the label.

The optional `description` field is displayed in the web UI as placeholder text and helper text below the input field.

### Supported types

| Type       | Description                                                    |
|------------|----------------------------------------------------------------|
| `string`   | Single-line text. Renders as an input field in the UI          |
| `text`     | Multiline text. Renders as a textarea in the UI                |
| `integer`  | Whole number                                                   |
| `number`   | Numeric value (integer or decimal)                             |
| `boolean`  | True/false. Renders as a checkbox in the UI                    |
| `date`     | Date value (`YYYY-MM-DD`). Renders as a date picker in the UI |
| `datetime` | Date and time. Renders as a datetime picker in the UI          |

Both `string` and `text` are treated identically at runtime — the difference is only in how the UI renders the input field. Use `text` for values that benefit from multiline editing such as SQL queries, scripts, or markdown content.

Both `date` and `datetime` are treated as strings at runtime. The `date` type produces values like `2026-01-15`, while `datetime` produces values like `2026-01-15T14:30`.

### Task-level input

Task input is provided when triggering the task via API, CLI, or trigger:

```bash
curl -X POST http://localhost:8080/api/workspaces/default/tasks/deploy-pipeline/execute \
  -H "Content-Type: application/json" \
  -d '{"input": {"env": "production"}}'
```

Or via CLI:

```bash
stroem trigger deploy-pipeline --input '{"env": "production"}'
```

### Default values

Fields with a `default` are automatically filled in when not provided by the caller. This works across all job creation paths (API, CLI, triggers, webhooks, hooks, and task actions).

Default values can use [Tera templates](/guides/templating/) with access to `secret.*` (workspace secrets):

```yaml
tasks:
  deploy:
    input:
      env:
        type: string
        default: "staging"
      api_key:
        type: string
        default: "{{ secret.DEPLOY_API_KEY }}"
    flow:
      run:
        action: deploy-app
        input:
          env: "{{ input.env }}"
          api_key: "{{ input.api_key }}"
```

When triggered without specifying `env` or `api_key`, the defaults are applied automatically. Non-string defaults (numbers, booleans) pass through unchanged.

Fields marked `required: true` without a default will produce an error if not provided.

### Secret inputs

Mark an input as `secret: true` to indicate it contains sensitive data:

```yaml
tasks:
  deploy:
    input:
      api_key:
        type: string
        secret: true
        default: "{{ secret.DEPLOY_API_KEY }}"
```

When `secret: true` is set and the field has a default value:

- The UI renders a password field with a masked placeholder (`********`)
- If the user submits without changing the value, the field is omitted from the input payload
- The server fills in the default value and Tera renders the secret reference at execution time

This prevents the raw template string (e.g. `{{ secret.DEPLOY_API_KEY }}`) from appearing in the UI while ensuring the real secret value is used at runtime.

### Dropdown options

Add `options` to any input field to render it as a dropdown in the UI:

```yaml
tasks:
  deploy:
    input:
      env:
        type: string
        options: [staging, production, dev]
        default: staging
      region:
        type: string
        name: AWS Region
        options:
          - us-east-1
          - eu-west-1
          - ap-southeast-1
        allow_custom: true
```

When `options` is set, the UI renders a select dropdown instead of a text input. The value submitted is always a string from the list.

Set `allow_custom: true` to let users type a custom value in addition to the predefined options. The UI renders a text input with autocomplete suggestions.

### Action-level input

Action input is provided by the step definition in the task flow. Values support [Tera templates](/guides/templating/):

```yaml
tasks:
  deploy:
    input:
      env: { type: string, default: "staging" }
    flow:
      deploy-step:
        action: deploy
        input:
          env: "{{ input.env }}"
```

## Structured output

Actions can emit structured output by printing a line with the `OUTPUT:` prefix followed by JSON:

```bash
#!/bin/bash
echo "Doing work..."
echo "OUTPUT: {\"status\": \"deployed\", \"version\": \"1.2.3\"}"
```

Only the **last** `OUTPUT: {json}` line is captured. The JSON is parsed and made available to downstream steps via templating:

```yaml
flow:
  build:
    action: build-app
  deploy:
    action: deploy
    depends_on: [build]
    input:
      version: "{{ build.output.version }}"
```

## Environment variables

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

## Organizing tasks with folders

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

Tasks without a `folder` property appear at the root level. When no tasks have folders, the UI shows a flat table.
