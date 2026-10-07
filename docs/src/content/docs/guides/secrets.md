---
title: Secrets & Encryption
description: SOPS encryption and vals secret resolution
---

Strøm supports two complementary approaches to secret management: **SOPS-encrypted workflow files** and **vals secret resolution** from external secret stores.

## SOPS encrypted files

Workflow files can be encrypted with [SOPS](https://github.com/getsops/sops). Name encrypted files with a `.sops.yaml` or `.sops.yml` suffix (e.g., `secrets.sops.yaml`). The server and CLI automatically detect these files and decrypt them before loading.

### Requirements

- `sops` must be installed and available on PATH
- Decryption keys must be configured (age, AWS KMS, GCP KMS, etc.)
- The `.sops.yaml` configuration file (if present) is ignored by the loader

### Example

```bash
# Encrypt a secrets file with age
sops -e --age age1... secrets.yaml > secrets.sops.yaml
```

The decrypted content is merged with other workflow files normally. Values from SOPS-encrypted files are already plaintext after decryption and don't need `| vals`.

## Secret resolution with `| vals`

The `| vals` filter resolves `ref+` secret references at template render time using the [vals](https://github.com/helmfile/vals) CLI.

### Prerequisites

The `vals` binary must be installed and available on PATH on the **server** (where templates are rendered).

### Workspace-level secrets

The `secrets:` section is rendered through Tera at **workspace load time**. Secrets are resolved once and cached in memory:

```yaml
secrets:
  DB_PASSWORD: "{{ 'ref+awsssm:///prod/db/password' | vals }}"
  API_TOKEN: "{{ 'ref+vault://secret/data/api#token' | vals }}"
  SLACK_WEBHOOK: "{{ 'ref+gcpsecrets://my-project/slack-webhook' | vals }}"
```

Then use the resolved values in templates — no `| vals` needed:

```yaml
env:
  DB_PASSWORD: "{{ secret.DB_PASSWORD }}"
script: "deploy --token {{ secret.API_TOKEN }}"
```

Secrets are re-resolved when the workspace reloads (on config change or git poll), so rotated secrets are picked up automatically.

### Inline resolution

You can also use `| vals` inline in any template expression (step `input:`, action `env:`, `script:`, `source:`, hook `input:`) without going through `secrets:`:

```yaml
env:
  DB_PASSWORD: "{{ 'ref+awsssm:///prod/db/password' | vals }}"
```

This resolves at step claim time rather than workspace load time.

### Full example

```yaml
secrets:
  DB_PASSWORD: "{{ 'ref+awsssm:///prod/db/password' | vals }}"
  API_TOKEN: "{{ 'ref+vault://secret/data/api#token' | vals }}"
  SLACK_WEBHOOK: "{{ 'ref+gcpsecrets://my-project/slack-webhook' | vals }}"

actions:
  deploy:
    type: script
    script: "deploy --token {{ secret.API_TOKEN }}"
    env:
      DB_PASSWORD: "{{ secret.DB_PASSWORD }}"
    input:
      env: { type: string }

tasks:
  deploy:
    flow:
      deploy:
        action: deploy
        input:
          env: "{{ input.env }}"
    on_success:
      - action: notify-slack
        input:
          webhook_url: "{{ secret.SLACK_WEBHOOK }}"
          message: "Deploy succeeded"
```

### Supported backends

Any backend supported by vals works:

- `ref+awsssm://` — AWS SSM Parameter Store
- `ref+vault://` — HashiCorp Vault
- `ref+gcpsecrets://` — Google Cloud Secret Manager
- `ref+azurekeyvault://` — Azure Key Vault
- `ref+sops://` — SOPS encrypted files
- And [many more](https://github.com/helmfile/vals#supported-backends)

### Behavior

- Plain strings (not starting with `ref+`) pass through unchanged — `{{ "hello" | vals }}` returns `"hello"`
- Non-string values (numbers, booleans) pass through unchanged
- If `vals` is not installed and a `ref+` value is encountered, the template render fails with a clear error
- Each `| vals` usage invokes the vals CLI once

### What is masked

Job detail, webhook and MCP responses mask secret values that appear in **strings** — including strings inside JSON input values. Numbers, booleans and `null` are never masked:

- Quote numeric secrets (`PORT: "5432"`) so they are masked; `stroem validate` warns about unquoted ones.
- A filter that changes a secret's form — `| int`, `| upper`, `| b64encode`, a slice — produces a value that is not masked, in any field type.
- Values of 3 characters or fewer are never masked.

### API redaction

Secret values are automatically redacted from API responses. When you view a job via `GET /api/jobs/:id`, every field except identifiers (ids, names, statuses, revisions, timestamps) — job input/output, step input/output, approval messages, conditions — that contains a known secret value will have it replaced with `••••••`. Substring matches are also redacted. Additionally, unresolved `ref+` references are redacted to avoid leaking secret-manager paths.

The same masking applies to a sync webhook's response and the webhook
job-status poll, to MCP `get_job_status`, and to the step errors listed on a
worker's detail page. A job that runs on a [git ref](/guides/git-refs/) is
also masked with the secrets of its commit (and of every commit it is
connected to); see [Git Refs → Secrets and redaction](/guides/git-refs/#secrets-and-redaction).
Job **logs** are not masked: a script that prints a secret shows it in the
log.

Step `error_message` and `retry_history` are redacted the same way. Template
errors never contain values: they are built from a fixed category, the
position in the template and at most a type name or the failing filter's name
— not Tera's own message text, not the rendered value, not the template line.
Errors about a value *after* rendering (for example a rendered connection name
that does not resolve) name the input field, never the value. As a second
layer, errors raised while rendering a step at claim time are also scrubbed
against the known secret values **before** they are written. To see Tera's full
report, run the workflow locally with `stroem run` or `stroem validate`. Limits: `stroem validate` compiles only `when`, `for_each` and agent prompts (plus secrets and connections at load), and `stroem run` only runs tasks made entirely of local `type: script` steps.

:::caution
`{{ obj }}` renders a whole object. `"dsn={{ input.db }}"` with a
connection-typed input writes every property of the connection — password
included — into the step input (Tera 1 wrote `[object]`). The value is masked
when the job is read if the property is `secret: true`, but it is still the
input the step receives. Reference the fields you need (`{{ input.db.host }}`).
:::
