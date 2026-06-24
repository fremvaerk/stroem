---
title: Artifacts
description: Produce, view, and download files from your tasks.
---

Artifacts let a task produce files — reports, screenshots, build outputs — that
humans can view or download from the UI, CLI, or MCP after the job completes.

## Producing artifacts

Write files to `/artifacts/` (or the `$ARTIFACTS_DIR` env var for portability):

```yaml
name: nightly-report
steps:
  - name: build
    action:
      type: script
      runner: local
      language: shell
      script: |
        ./tools/report --out "$ARTIFACTS_DIR/report.html"
        cp build/dist.zip "$ARTIFACTS_DIR/"
```

When the step succeeds, the worker uploads every file in `/artifacts/` (recursively). Failed or cancelled steps discard their `/artifacts/` directory.

### What gets uploaded

- Regular files (recursive — `reports/q1.html` keeps the slash in its name).
- Dotfiles (e.g. `.config`). **Don't put `.env` here.**
- Empty files.
- Symbolic links are skipped + warned.
- Filenames with `..`, null bytes, or control characters are rejected + warned.

### Limits

| | Default | Configurable |
|---|---:|---|
| Per-file | 100 MiB | `artifact_storage.max_file_bytes` |
| Per-job  | 1 GiB   | `artifact_storage.max_job_bytes` |

Exceeding either fails the step loudly.

### Runner support

| Runner | Supported | What `$ARTIFACTS_DIR` points to |
|---|---|---|
| Shell (local) | Yes | A fresh per-step host tempdir (e.g. `/tmp/.tmpXxx/`). The worker creates it via `tempfile::TempDir`, keeps it alive through the post-success scan/upload, then auto-deletes when the step completes. |
| Docker (`script:docker`) | Yes | `/artifacts` (container path) — the worker bind-mounts a host tempdir to that mount point. |
| Docker (`type:docker`) | Yes | `/artifacts` |
| Kubernetes (any) | No (deferred — same limitation as state `/state-out`) | unset |
| Agent / Approval / Task actions | n/a | unset |

On unsupported runners, `$ARTIFACTS_DIR` is unset; scripts that try `> "$ARTIFACTS_DIR/foo"` will fail loudly with "ambiguous redirect".

You can also reference the directory via the Tera path-variable `{{ artifacts_dir }}` (renders to `$ARTIFACTS_DIR`). See [Action types — Path variables](../action-types/#path-variables) for the full list including `{{ state_dir }}` and friends.

## Viewing & downloading

UI: a new **Artifacts** section appears near the top of the Job Detail page. Safe MIME types (PNG, JPEG, GIF, WebP, PDF, plain text, Markdown) open inline in a new tab. Everything else — including HTML and SVG (for XSS hardening) — downloads.

CLI:

```sh
stroem-api artifacts list <job-id>
stroem-api artifacts download <job-id> report.html -o ./report.html
```

Hooks: `hook.artifacts` is a list of `{name, content_type, size_bytes, url, step_name, created_at}`. Example Slack notification:

```yaml
on_success:
  - action: slack-notify
    input:
      text: |
        Job done. Outputs:
        {% for a in hook.artifacts %}- <{{ a.url }}|{{ a.name }}> ({{ a.size_bytes }} B)\n{% endfor %}
```

MCP: `list_artifacts(job_id)` and `get_artifact(job_id, name)` (text/JSON/YAML/XML up to 1 MB only — binaries should be linked via URL, not embedded in the agent's context).

## Retention

Artifacts live as long as the job row does. Once a job is deleted (via the existing `retention.job_days` sweep), its blobs and rows go with it. There is no separate artifact TTL today.

## Security notes

Artifacts produced by a workflow are served with the stored Content-Type, but anything that could execute in the browser (HTML, SVG, XML) is force-downloaded rather than rendered. `X-Content-Type-Options: nosniff` blocks browsers from second-guessing.

`View` permission on the task is sufficient to list and download artifacts — the same permission required to view logs. Don't put secrets in `/artifacts/`.
