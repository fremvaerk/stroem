---
title: File Inputs
description: Give a task a file. Upload from the UI, REST API, CLI or MCP, and read it in a step through a path
---

A `type: file` input field lets a person or a program give a task a file. The step does not get the content in a template value. It gets a **path** to a read-only copy of the file, for example as a command-line argument.

:::caution[Rollout order]
An older server replica treats `type: file` as a [connection type](/guides/connections/) and answers `400` for such a task. Upgrade **every server replica first, then every worker, then** add YAML with `type: file`. See [Migration 054](/operations/migration-054/).
:::

## Declaring a file field

```yaml
actions:
  show-file:
    type: script
    script: |
      sha256sum "{{ input.data_file }}" && cat "{{ input.data_file }}"
    input:
      data_file: { type: file, required: true }

tasks:
  file-demo:
    mode: distributed
    description: Demonstrates the file input type.
    input:
      data_file:
        type: file
        required: true
        description: Any file
    flow:
      show:
        action: show-file
        input:
          data_file: "{{ input.data_file }}"
```

In a template, `{{ input.data_file }}` renders to a path such as `/inputs/file_7f3a91c2…/report.csv`. The file is read-only. The stored value of the field (in the job input, the API and hook payloads) is the **file ID**, never the path.

`type: file` is allowed in a task `input` and in an action `input`. `stroem validate` rejects:

- a `file` field that has `default`, `options`, `allow_custom`, `multiple` or `secret`;
- a `file` property in a [connection type](/guides/connections/), and a connection type named `file` (the name is reserved);
- a `file` field in an `approval` action;
- an agent action whose `tools` name a task with a `required` file field (an agent cannot supply a file).

It warns when a task with a `required` file field is the target of a scheduler trigger, an event source trigger or a webhook in the same workspace (a trigger with `ref:` is not checked), because none of them can supply a file.

The server does not run these checks when it loads a workspace. A `default` on a `file` field that reaches the server is not attached to the job: the template gets the literal default, and the step gets no file.

A `required` file field with no value fails job creation in every creation mode. This differs from other types, which the server does not check.

### Passing a file to an action or a child task

A flow step gives the file on with a template:

- `input: { path: "{{ input.data_file }}" }` gives a `string` action field the path.
- `input: { data: "{{ input.data_file }}" }` gives a `file` action field the same file. In the action body `{{ input.data }}` is the same path.

An action `file` field also accepts a file ID, or a path of a file that belongs to the same job. Any other value fails the step with `Input field 'data': the file cannot be used`.

A `type: task` step passes the file to the child job's `file` field in the same way, in the same or in another workspace. Only a child `file` field attaches the file to the child job: a child field of another type that gets `{{ input.data_file }}` receives the path `/inputs/{file_id}/{name}` as text, and the child's steps cannot read that path. A job input file field with the same name as an unset action field is copied by the target type: the file ID into a `file` action field, the path into any other field.

Re-run, Restart and [task retry](/guides/retry/) keep the file of the source job. Hook jobs and agent task-tool children do not get it. The tool schema that an agent sees for a task leaves out the task's `file` fields; a tool call for a task with a `required` file field fails with `Input field '…': a file is required`.

## Supply a file

Every client works in two calls: **upload** the file (you get a file ID), then **execute** the task with the ID as the field value.

### UI

The Run form shows a file picker for a `file` field. The browser uploads the file when you select it, with a progress bar. The form shows the file name and size, and offers **Remove** and **Replace**. **Run** is disabled while an upload runs or after it fails. A failed upload offers **Remove**. **Cancel** of a **Replace**, and **Remove** after a failed **Replace**, return to the file the field had before.

On the job page, the **Input files** card lists the files of the job with a download link.

On [Re-run](/guides/rerun-and-restart/#files), the field shows the file of the source job. Keep it, or choose **Replace** to upload another file. If you cancel the **Replace**, or remove a failed replacement, the field shows the file of the source job again.

### REST API

```sh
# 1. Upload the raw bytes (no multipart)
curl -X POST \
  -H "Authorization: Bearer $STROEM_TOKEN" \
  -H "Content-Type: text/csv" \
  --data-binary @report.csv \
  "http://localhost:8080/api/workspaces/default/tasks/import-report/files?name=report.csv"
# {"file_id":"file_7f3a…","name":"report.csv","size_bytes":1234,"sha256":"…",
#  "content_type":"text/csv","expires_at":"2026-10-31T12:00:00Z"}

# 2. Execute with the file ID
curl -X POST \
  -H "Authorization: Bearer $STROEM_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"input": {"data_file": "file_7f3a…"}}' \
  http://localhost:8080/api/workspaces/default/tasks/import-report/execute
```

See the [API reference](/reference/api/#upload-input-file). You can upload once and execute several times within the upload lifetime.

### CLI

```sh
stroem-api trigger import-report --file data_file=./report.csv
```

`--file FIELD=PATH` can be repeated. The CLI uploads each file, then executes the task with the IDs. A key that you give in both `--input` and `--file` is an error.

`stroem run` runs a task locally without a server. The value of a `file` field is a local path. The CLI checks that it is a regular file and copies it read-only to `{temp}/stroem-run-inputs-{uuid}/{file_id}/{safe_name}`, so the path in the script has the same shape as on the server. Only your user can open `stroem-run-inputs-{uuid}` (mode `0700`), so other users of the host cannot read the copy. The directory is removed when the run ends:

```sh
stroem run import-report --input '{"data_file": "./report.csv"}'
```

`stroem run` still runs only `type: script` with `runner: local`.

### MCP

An agent has two ways to send a file with `execute_task`:

- **Inline.** The `file` field is an object `{"name": "report.csv", "content": "<utf-8 text>"}` or `{"name": "report.csv", "content_base64": "<base64>"}`, with exactly these two keys. Any other shape, or base64 that does not decode, gives `Input field 'f': the file cannot be used`. The decoded size must not be more than 1 MiB (`input_files.mcp_inline_max_bytes`). A larger file gives `Input field 'f': inline content is larger than N bytes`. The server stores the file as an upload of the caller (content type `application/octet-stream`), then creates the job.
- **Upload URL.** Call `create_upload_url(workspace, task_name, file_name)`. It needs the Run permission on the task, a task with a `file` field, and a usable file name. It returns `{upload_url, method: "POST", max_bytes, expires_at}`. `POST` the raw bytes to the URL **without** a Bearer header: the URL token is the credential. The URL is valid for 15 minutes (`input_files.upload_url_ttl_secs`) and works **once**: the token is used up when the request arrives, before the body is read, so if an upload fails, ask for a new URL. A used, expired or unknown URL gives `404 Upload URL not found`. The response is the same as for the REST upload, and the upload belongs to the user who asked for the URL. Then call `execute_task` with the returned `file_id`.

`create_upload_url` needs `auth.base_url` in the server config. Without it the tool answers `upload URLs need auth.base_url; send the file inline or use the REST upload`. See the [MCP guide](/guides/mcp/).

## Where the file is in the step

The path is `{root}/{file_id}/{safe_name}`. The path depends on the file only, never on the field that holds it, so one file has one path in every template of a step, and two files never share a path.

| Runner | Root |
|---|---|
| `runner: local` | `{temp}/stroem-inputs-{uuid}/in`, where `{temp}` is the worker's temporary directory (`TMPDIR`). New for each claim and removed after the step. The parent `stroem-inputs-{uuid}` has mode `0700`, so other users of the host cannot reach the files |
| `runner: docker`, `type: docker` | `/inputs` (read-only bind mount) |
| `runner: pod`, `type: pod` | `/inputs` (read-only volume) |

In a context that no worker renders (an agent prompt, a `when` or `for_each` expression, a `type: task` step input, an approval message, a [templated ref](/guides/git-refs/)), the path uses `/inputs`. To test whether a file was given to an optional field, write `when: "{{ input.data_file | default(value='') }}"`: an absent field is a missing key, and a missing key is a template error that fails the step. A `for_each` item that holds a file path gets the step's own root at claim, so `{{ each.item }}` is a valid path on every runner.

On a pod, a separate init container `stroem-inputs-init` downloads each file from the server to an `emptyDir` volume `stroem-inputs`, then checks the size and the SHA-256. The main container mounts the volume read-only at `/inputs`. The init container uses the same image and retry policy as `workspace-init`, and is added only when the step has files. If a download or a check fails, the pod does not start the main container and the step fails. A pod manifest override can rewrite both names, as for `workspace-init`; Strøm does not validate that. For the other runners, the worker downloads the file, streams it to disk, and checks the size and SHA-256 before the runner starts. The file has mode `0444` and its directories `0555`, so a Docker step that runs as another user can still read it. A `503` from the server is retried every 5 seconds, up to 12 attempts.

A local step sees a path such as `/tmp/stroem-inputs-5d0c…/in/file_7f3a…/report.csv`. A retried step is claimed again with a new directory, and its paths are rendered again for that directory.

### Every step gets every file

**Every runner step of a job receives every file of that job**, not only the files the step uses. A step that does not mention a file still finds it in `/inputs` (or in the local root). Agent steps (`type: agent`) have no runner and receive no files.

This has consequences:

- A step that runs an action from **another workspace** ([cross-workspace reference](/guides/cross-workspace-references/)) receives all files of the caller job. The script of that workspace can list and read them. If you use another workspace's action in a task that has `file` fields, you trust that action with those files.
- A file that you give to a `type: task` child in another workspace can be downloaded by everyone who has View permission on the child job.
- Every instance of a [`for_each`](/guides/loops/) step downloads every file. On a pod runner, each instance pod runs its own init container.

## Limits, lifetime and names

| Setting | Default |
|---|---|
| Largest file (`input_files.max_file_bytes`) | 100 MiB. A larger upload gives `413` and nothing is kept |
| Lifetime of an unused upload (`input_files.upload_ttl_secs`) | 24 hours |
| MCP upload URL lifetime (`input_files.upload_url_ttl_secs`) | 15 minutes, single use |
| Largest inline MCP file (`input_files.mcp_inline_max_bytes`) | 1 MiB decoded |

See [Storage](/operations/storage/#input-files) for the configuration.

**Who can use an upload.** An upload belongs to the user who made it (an API key or an MCP OAuth token counts as its user) and to one task. A direct start of the task can use it: the UI, the REST API, `stroem-api`, MCP `execute_task`, Re-run and Restart. The upload must be younger than the lifetime above. With authentication off, any caller can use an upload. A file that is already attached to an earlier job can also be reused by the jobs that derive from it (Re-run, Restart, task retry and `type: task` children), with no time limit. A trigger, a webhook, an event source or a hook cannot use an upload. Any other case fails with `Input field 'data_file': the file cannot be used`. The message never contains the ID, the name or the path.

**Safe name.** The step sees a safe version of the file name. The original name stays on the job page and in downloads. The rule:

- Keep the base name only.
- Replace every character outside `A-Z a-z 0-9 . _ -` with `_`.
- Replace a leading `.` or `-` with `_`.
- Keep at most 128 bytes, and keep the extension.
- A missing or empty `name`, or a name that is `.` or `..` (or ends in `/`), gives `400 invalid file name`.

| Original name | Safe name |
|---|---|
| `report.csv` | `report.csv` |
| `x;rm -rf ~.csv` | `x_rm_-rf__.csv` |
| `-rf` | `_rf` |
| `.env` | `_env` |
| `my report (final).pdf` | `my_report__final_.pdf` |

The rule keeps the path safe to put into `script:`, `cmd:` and `args:`.

## What v1 does not support

- A webhook, a scheduler trigger, an event source, a hook or an agent task tool cannot supply a file. When the target task has a `required` file field, or the trigger's `input:` gives a `file` field a value, the scheduler logs `Trigger '…' MISSED` and a webhook answers `400`. The check runs after the target is resolved and before the concurrency policy, so nothing is skipped or cancelled. An event source cannot create a job for such a task, and a hook whose action or task needs a file is not fired (the error is logged to the job that fired it). An optional `file` field is simply absent in all these jobs.
- One file for each field.
- The file name, size and content type are not available in templates, only the path.
- The content of a file is not masked. Do not rely on secret masking for file content.
- `stroem run` runs local scripts only.

## Limitations

- **Helm with the Docker-in-Docker sidecar.** The `dind` sidecar does not share a volume with the worker. A `runner: docker` step sees an **empty** `/inputs` (and an empty `/workspace` and `/state`), because the daemon creates a missing bind source as an empty directory on its own file system. In such a deployment, use `runner: local` or `runner: pod` for steps that read input files.
- **Pod init failures.** When `stroem-inputs-init` fails, the step shows only the existing pod failure (exit code 1), with no text about the cause. The init container writes one line (the file ID and the check that failed) to its own log, which is not copied to the step log.
- **Large files on pods.** The init download counts toward the pod's 10-minute pending limit. Several large files, or a slow server, can reach it; the step then fails with `Pod stuck in Pending state …`.

## Retention

A file lives as long as a job uses it. When job retention deletes a job, the links of the job to its files go with it. A file that no job uses is deleted by the retention sweep once it is older than `upload_ttl_secs`, together with expired upload URLs. The sweep runs on every retention tick (`retention.interval_secs`, hourly by default), also when no `retention.job_days` or `worker_hours` is set. See [Retention](/operations/retention/#input-files).

## Troubleshooting

| Message | Cause |
|---|---|
| `Input field 'f': the file cannot be used` | The ID is unknown, belongs to another user or task, has expired or is being deleted, or the value is not a file ID |
| `Input field 'f': a file is required` | A `required` file field has no value |
| `invalid file name` | The `name` is missing or empty, or it is `.` or `..` |
| `task has no file input` (`400`) | The upload is for a task that declares no `file` field |
| `Source job not found` (`404`) | An upload with `source_job_id`: the job does not exist, or you may not view it |
| `could not load the job's input files` | The server could not read the job's files when a worker claimed the step (a database error). Step `retry:` applies |
| `file is larger than N bytes` (`413`) | The file is over `input_files.max_file_bytes` |
| `file storage is not configured` (`503`) | The server has no archive for blobs. It refuses uploads, and every run with a file value, also a Re-run or Restart of a job with a file. A step of a job that already has files fails when a worker claims it. See [Storage](/operations/storage/#input-files) |
| `this step has file inputs, which the worker does not support; upgrade the worker` | An old worker claimed a step of a job that has files. Upgrade the workers. Step `retry:` applies |
| `Input field 'f': inline content is larger than N bytes` | An MCP inline file is over the inline limit. Use `create_upload_url` |
| `upload URLs need auth.base_url; …` | Set `auth.base_url`, or send the file inline |
| `Upload URL not found` (`404`) | The URL was used, expired or never existed |
