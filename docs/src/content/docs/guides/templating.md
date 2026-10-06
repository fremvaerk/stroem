---
title: Templating
description: Tera template engine, variables, and step name rules
---

Strøm uses [Tera 2](https://keats.github.io/tera/) for templating. Templates are rendered on the server (when a worker claims a step, and for task actions, `when`, `for_each`, hooks and approval messages) and by the `stroem` CLI — never on the worker itself. Coming from Tera 1? See the [upgrade guide](/operations/upgrade-tera-2/).

## Available context

Every template field a step can carry sees the same variables. There is one
rule, not one per field.

| Variable | Value | Notes |
|---|---|---|
| `input.*` | job input — or, in action bodies (`script`, `cmd`, `env`, `args`, `manifest`, `image`), the step's resolved input | approval `message:` sees the step's resolved input when the step has an input mapping, else job input |
| `secret.*` | workspace secrets (the action's **owner** workspace in action bodies; the job's workspace elsewhere) | always present, `{}` when none |
| `state.*` / `global_state.*` | the latest task / global state snapshot's `state.json` | `null` until a parsed snapshot exists — `{{ not state }}` is true whenever no parsed `state.json` is available (no snapshot yet, a snapshot without a sidecar, or one whose sidecar did not parse), and `{{ state.x \| default(value=0) }}` falls back to the default |
| `job.revision` | workspace revision pinned at creation — for a [pinned job](/guides/git-refs/), the commit its ref resolved to | always present, `null` for pre-migration jobs |
| `job.ref` | the [git ref](/guides/git-refs/) a pinned job runs at, as written (`release/2.3`, `v4.1.0`, a commit SHA) | always present, `null` (renders as an empty string) for every job that does not run on a ref |
| `<step>.output` | a finished step's output (`null` when a completed step produced none; `null` for skipped, failed and suspended steps — either renders as an empty string) | hyphens in step names become underscores |
| `<step>.error` | a failed step's error message | |
| `each.item` / `each.index` / `each.total` | loop variables inside a `for_each` instance | not available in `when:` — the condition runs before the loop expands |

**Reserved names.** A flow step named `input`, `secret`, `state`,
`global_state` or `job` shadows that variable; a step named `each` is shadowed
by the loop variable. When a worker claims the step, the server writes a
`[render] step '…' shadows template variable '…'` line to the job log; for
`when:`, `for_each:`, `type: task` inputs and approval messages the collision
is recorded in the server log only. Avoid these names.

A step named after a Tera 2 keyword — `none`, `null`, `self`, `loop`, `break`,
`continue`, `true`, `false`, `and`, `or`, `not`, `is`, `in`, `if`, `else` —
cannot be referenced from a template at all. The server logs
`[render] step '…' is a Tera keyword and cannot be referenced in templates`.
Rename the step.

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

The value is identical for every step of a job (sub-jobs and hook jobs inherit the parent's revision — except a child that runs another workspace's task, or its own [`ref:`](/guides/git-refs/)). For jobs created before revision tracking existed it renders as an empty string. In hooks, the same value is available as `hook.revision` — see [Hooks](/guides/hooks/).

`{{ job.ref }}` is the ref a [pinned job](/guides/git-refs/) runs at —
`release/2.3`, `v4.1.0`, or a commit SHA as written — and an empty string
for every other job; `{{ job.revision }}` of a pinned job is the resolved
commit. Hooks get the same value as `hook.ref`.

```yaml
actions:
  report:
    type: script
    script: "echo Running {% if job.ref %}ref {{ job.ref }}{% else %}the default branch{% endif %} at {{ job.revision }}"
```

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

See the [Tera 2 documentation](https://keats.github.io/tera/) for the full feature set.

### Undefined values and `default`

- Only the **last** path segment may be undefined: `{{ a.b | default(value=1) }}`
  works when `b` is missing, `{{ a.b.c | default(value=1) }}` is an error when
  `b` is missing. Use optional chaining: `{{ a.b?.c | default(value=1) }}`.
- `default` replaces both undefined **and `null`** values, so
  `{{ step.output.x | default(value='n/a') }}` works over a skipped, failed or
  suspended step (whose `output` is `null`). Strøm keeps this Tera 1 behaviour on
  top of Tera 2.
- `state` and `global_state` are `null` before the first snapshot, so
  `state.x | default(value=0)` is safe on a task's first run.
- `and` / `or` return one of their operands, not a boolean.
- Render arrays and objects with `| json_encode()` when you need JSON; a bare
  `{{ obj }}` renders `{"k": v}` and a string array renders `["a", "b"]`.

### Template errors

Template errors are **value-free**: they carry a category, the position
(`line L, column C`) and at most a type name or the failing filter's name —
never the rendered value, a variable name or the template line — so they are
safe in job logs, step errors and API responses. For the full Tera report run
the same workspace locally with `stroem run` or `stroem validate`. Limits: `stroem validate` compiles only `when`, `for_each` and agent prompts (plus secrets and connections at load), and `stroem run` only runs tasks made entirely of local `type: script` steps.

The Tera 1 filters `urlencode`, `urlencode_strict`, `slugify`,
`filesizeformat`, `striptags`, `addslashes` and `get_env`, plus the `spaceless` filter, the `is matching`
test and `get_random()`, are not available;
see the [upgrade guide](/operations/upgrade-tera-2/#11-filters-that-were-not-restored).

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
        depends_on:
          - step: check
            accept: [completed, skipped]
        # Runs whether check ran or was skipped — the accept set that lets
        # it through lives on `process`'s own edge, the dependent, not on
        # `check`.
```

See [Conditional Flow Steps](/guides/conditionals/) for the full feature documentation.
