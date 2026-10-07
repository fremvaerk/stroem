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

The optional `description` field is displayed in the web UI as helper text below the input field. The placeholder inside an empty input shows the field key (or "Select …" for option fields), never the description.

### Supported types

| Type       | Description                                                    |
|------------|----------------------------------------------------------------|
| `string`   | Single-line text. Renders as an input field in the UI          |
| `text`     | Multiline text. Renders as a textarea in the UI                |
| `integer`  | Whole number                                                   |
| `number`   | Numeric value (integer or decimal)                             |
| `boolean`  | True/false. Renders as a checkbox in the UI. Also accepts `bool` as an alias |
| `date`     | Date value (`YYYY-MM-DD`). Renders as a date field in the UI   |
| `datetime` | Date and time (`YYYY-MM-DDTHH:MM`). Renders as a date field plus a time field in the UI |
| `json`     | Any JSON value (object, array, string, number, boolean, null). Renders as a JSON editor in the UI |

If the `type` is not one of the types above, it is treated as a [connection type](/guides/connections/) reference. The UI renders a searchable dropdown of matching connections in the workspace.

Both `string` and `text` are treated identically at runtime — the difference is only in how the UI renders the input field. Use `text` for values that benefit from multiline editing such as SQL queries, scripts, or markdown content.

Both `date` and `datetime` are treated as strings at runtime. The `date` type produces values like `2026-01-15`, while `datetime` produces values like `2026-01-15T14:30`.

In the UI, type the date as `YYYY-MM-DD` (for example `2026-01-15`; `2026-1-5` is accepted and tidied to `2026-01-05`), or pick it from the calendar button at the end of the field. The calendar has month and year dropdowns (100 years back to 20 years ahead) and a **Today** button. Text that isn't a real date, such as `2026-02-31`, is marked invalid and the form won't submit until it is fixed or cleared.

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

#### Empty fields in the UI

A field left empty in the **Run task** form is treated the same way as an API call that omits the key. If the field has no `default`, the key is not sent at all, so `{{ input.field | default(value='x') }}` applies the fallback and a plain `{{ input.field }}` fails to render, exactly as it would for an API, CLI or MCP caller that left the field out. If the field *has* a `default`, clearing it sends an empty string, which is an explicit override.

Tera's `default` filter fires for an *absent* or `null` variable, never for an empty string, so a template that must tolerate both should test the value: `{% if input.field %}{{ input.field }}{% else %}x{% endif %}`.

### JSON inputs

A `json` field holds any JSON value and keeps it structured through templates.

```yaml
actions:
  deploy:
    type: script
    script: echo "{{ input.cfg.region }} x{{ input.replicas + 1 }}"
    input:
      cfg: { type: json }
      replicas: { type: json }
tasks:
  release:
    input:
      targets: { type: json, default: [eu, us] }
    flow:
      plan: { action: make-plan }
      go:
        action: deploy
        depends_on: [plan]
        input:
          cfg: "{{ plan.output.cfg }}"            # an object
          replicas: "{{ plan.output.hosts | length }}"   # a number
```

Rules for every string inside a `json` value (also inside its objects and arrays):

- **Literal text** (no `{{`, `{%` or `{#`) is used as is.
- **Exactly one `{{ expression }}`** takes the expression's value — object, array, number, boolean, string or `null`. A missing field (`{{ obj.missing }}`) or undefined variable (`{{ typo }}`) is an error, as everywhere; use `{{ obj.missing | default(value=none) }}` for an optional value (gives `null`).
- **Anything else** (`"id {{ x }}"`, two expressions, `{% if %}` blocks) is an error. Build text inside one expression instead: `{{ 'id ' ~ x }}`.
- `| json_encode()` gives JSON *text* — the field then holds a string. Leave it out to pass the value; `stroem validate` warns about it.
- **Known limitation:** a `}}` or `{{` inside a string literal of the expression (e.g. `{{ x | default(value="}}") }}`) makes the string count as more than one expression. Use a variable instead.

`json_encode` belongs in a script body, not in the field. To pass a whole json input to a script as JSON text, write `{{ input.cfg | json_encode() }}` there; a bare `{{ input.cfg }}` in a script renders Tera's own format, which is not JSON.

Values sent through the API, a webhook's `body`, the CLI `--input`, MCP and agent tools are used exactly as given. A `json` field cannot be `secret`, have `options`/`allow_custom`/`multiple`, or be used in an approval form.

In the **Run task** form a `json` field is a JSON editor. A default that contains templates is shown read-only ("evaluated when the job runs"); *Override* opens an empty editor. An empty editor sends nothing (the default applies) — type `""` or `null` to send those values. On a re-run the field starts in *replay*: the previous run's value is reused exactly; *Edit* opens it in the editor. A value typed in the form is parsed by the browser, so integers beyond 2^53 lose precision there — use the API or replay for those.

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

### Multi-select

Set `multiple: true` (alongside `options`) to let users pick more than one value. The UI renders a searchable popover with a checkbox per option, and the submitted value is a JSON array.

```yaml
tasks:
  deploy:
    input:
      environments:
        type: string
        options: [dev, staging, prod, sandbox]
        multiple: true
        default: [dev, staging]
        required: true
        description: Target environments
```

Notes:

- `default`, if present, must be a JSON array. Each item must appear in `options` unless `allow_custom: true`.
- `allow_custom: true` works the same way as for single-select — users can type values not in the list and add them to the selection.
- `multiple: true` is not supported on connection-type fields, and cannot be combined with `secret: true`.
- Inside templates the value is a list — use Tera's array filters, e.g. `{{ input.environments | join(sep=', ') }}` or iterate with `{% for env in input.environments %}`.

### Display order

By default, input fields appear in YAML key order (which may vary since YAML maps are unordered). Use `order` to control the display order in the UI:

```yaml
tasks:
  report:
    input:
      end_date:
        type: date
        name: End date
        order: 2
      start_date:
        type: date
        name: Start date
        order: 1
      format:
        type: string
        default: csv
```

Fields with lower `order` values appear first. Fields without `order` appear after all ordered fields, in their original key order.

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
    type: script
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
