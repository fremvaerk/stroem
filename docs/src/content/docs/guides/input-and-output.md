---
title: Input & Output
description: Input parameters, structured output, and organizing tasks with folders
---

## Input parameters

Both actions and tasks can declare input parameters.

```yaml
input:
  name:
    type: string          # single-line text
    required: true         # fails if not provided
  env:
    type: string
    default: "staging"     # used when not provided
  query:
    type: text            # multiline text (textarea in UI)
```

### Supported types

| Type      | Description                                           |
|-----------|-------------------------------------------------------|
| `string`  | Single-line text. Renders as an input field in the UI |
| `text`    | Multiline text. Renders as a textarea in the UI       |
| `integer` | Whole number                                          |
| `number`  | Numeric value (integer or decimal)                    |
| `boolean` | True/false. Renders as a checkbox in the UI           |

Both `string` and `text` are treated identically at runtime — the difference is only in how the UI renders the input field. Use `text` for values that benefit from multiline editing such as SQL queries, scripts, or markdown content.

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
