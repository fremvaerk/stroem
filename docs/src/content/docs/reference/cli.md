---
title: CLI
description: Command-line interface reference
---

Strøm provides two CLI binaries:

- **`stroem`** — Local workspace tool for exploring and running tasks without a server
- **`stroem-api`** — Remote server client for triggering jobs and querying a running Strøm server

## `stroem` (Local)

Local workspace commands. No server required.

```bash
# Set the workspace path (default: current directory)
stroem --path /path/to/workspace <command>
```

### `run`

Run a task locally without a server. Loads the workspace, walks the task's DAG, and executes each step via the local shell runner. Only `type: script` with `runner: local` (default) is supported — docker, pod, task, agent, and approval steps are rejected upfront.

```bash
# Run a task in the current directory
stroem run my-task

# Run from a specific workspace path
stroem run my-task --path /path/to/workspace

# Run with input
stroem run deploy --input '{"env": "staging"}'

# A file input field takes a local path
stroem run import-report --input '{"data_file": "./report.csv"}'
```

For a [`file` input field](/guides/file-inputs/) the value in `--input` is a local path. The CLI checks that it is a regular file and copies it read-only to `{temp}/stroem-run-inputs-{uuid}/{file_id}/{safe_name}`, so the path in the script has the same shape as on a server. The directory is removed when the run ends. A required `file` field with no value fails with `Input field '…': a file is required`.

Supports:
- DAG dependency ordering
- Template rendering (`{{ input.* }}`, `{{ step.output.* }}`, `{{ secret.* }}`)
- `when` conditions (skip steps based on expressions)
- `for_each` loops (iterate over arrays)
- The same dependency gate as the server: a step runs only when its whole `depends_on` tree is satisfied — each edge's `accept` set (default: must `complete`) names which of the dependency's outcomes let it through, independent of any flag on the dependent itself. An untolerated failure no longer aborts the whole run — independent branches keep going, and unreached dependents are skipped `unreachable`, matching server jobs
- A `when` or `for_each` evaluation error fails that step, same as the server, instead of aborting the run
- Masked step context for downstream templates: a completed step's output, `null` for a skipped one, `null` plus `error` for a failed one — a failed loop's partial array is never exposed to a step that reads it
- `Ctrl+C` graceful cancellation (still aborts the run and exits non-zero — there is no step-level cancellation locally)
- `OUTPUT: {json}` parsing for step outputs
- Exit code reflects the run's outcome, not a raw failure count: `0` when every failed step's own `continue_on_failure` catches it, `1` when at least one failed step escaped uncaught (a downstream step's `continue_on_failure`/`accept` never excuses someone else's failure). When every failure was caught, the run prints one extra line after the summary counts: `Every failure was caught by continue_on_failure.`

Limitations:
- Steps execute sequentially, even when the DAG allows parallelism
- `for_each` iterations always run sequentially regardless of the `sequential` setting
- Only `type: script` with local runner is supported — docker, pod, task, agent, and approval steps are rejected
- Summary counts are diagnostic only: a successful loop counts as one step in the tally, while a failed loop's instances are each counted individually

### `validate`

Validate workflow YAML files before deploying.

```bash
# Validate current directory
stroem validate

# Validate a specific path
stroem validate /path/to/workspace/.workflows/
```

The validator checks:
- YAML syntax and structure
- Action type validity
- Runner field validity
- Flow step and dependency references
- DAG cycle detection
- Trigger cron expression syntax
- Hook action references
- Dependency shape: a leftover `continue_when_skipped` flag, or a malformed `depends_on` tree (duplicate siblings, empty `all`/`any` groups, empty or duplicate-entry `accept` lists) — a named, actionable error, not a silent warning (see the [0.18 upgrade guide](/operations/upgrade-0-18-dependency-conditions/)). `stroem run` and job creation on the server enforce the same check; loading a workspace on its own does not.
- `ref:` values ([Git Refs](/guides/git-refs/)): the ref's syntax, and that it appears only where it is supported (flow-step actions, `type: task` actions, scheduler/webhook triggers). A reference with `ref:` is not resolved offline — it is skipped with a warning, like a library action

### `tasks`

List all tasks in the workspace.

```bash
stroem tasks
stroem --path /path/to/workspace tasks
```

### `actions`

List all actions in the workspace.

```bash
stroem actions
```

### `triggers`

List all triggers (scheduler and webhook) in the workspace.

```bash
stroem triggers
```

### `inspect`

Show detailed information about a single task: input schema, flow steps with dependencies, hooks.

```bash
stroem inspect deploy-pipeline
```

## `stroem-api` (Remote)

Remote server commands. Requires a running Strøm server.

```bash
# Set the server URL (default: http://localhost:8080)
export STROEM_URL=http://localhost:8080

# Set authentication token (optional)
export STROEM_TOKEN=strm_your_api_key
```

### `trigger`

Execute a task and create a new job.

```bash
# Trigger with default input
stroem-api trigger hello-world

# Trigger with input
stroem-api trigger hello-world --input '{"name": "CLI"}'

# Trigger in a specific workspace
stroem-api trigger etl-pipeline --workspace data-team --input '{"date": "2025-01-01"}'

# Give a file to a file input field (repeatable)
stroem-api trigger import-report --file data_file=./report.csv
```

`--file FIELD=PATH` uploads the file (streamed), then executes the task with the file ID as the value of `FIELD`. A key that appears in both `--input` and `--file` is an error. See [File Inputs](/guides/file-inputs/).

### `status`

Check the status of a job. The summary shows the job's input and output and the output of each step.

```bash
stroem-api status <job-id>

# Print the raw job detail (the JSON of GET /api/jobs/{id})
stroem-api status <job-id> --json
```

Secret values are masked (`••••••`), as on the job page.

### `logs`

View the logs of a job. Prints the last 256 KiB of the log (whole lines) by default and, when the log is longer, a note on stderr:

```
note: showing the last 256.0 KiB of up to 83.3 MiB; use --full for the whole log
```

```bash
# Tail (default: last 256 KiB)
stroem-api logs <job-id>

# Stream the whole log
stroem-api logs <job-id> --full

# Ask for a different tail size (max log_storage.read.tail_max_bytes, 4 MiB by default)
stroem-api logs <job-id> --tail-bytes 1048576
```

`--full` and `--tail-bytes` are mutually exclusive.

Against a server older than this release, `--full` receives the old full-log JSON body (no streaming) and holds it in memory before printing, instead of streaming NDJSON.

### `jobs`

List recent jobs.

```bash
# List last 10 jobs
stroem-api jobs --limit 10
```

### `tasks`

List tasks from the server (across workspaces).

```bash
# List all tasks
stroem-api tasks

# Filter by workspace
stroem-api tasks --workspace data-team
```

### `cancel`

Cancel a running or pending job.

```bash
stroem-api cancel <job-id>
```

### `restart`

[Restart](/guides/rerun-and-restart/) a finished, top-level job: from one step, or from every failed, cancelled or unfinished step. Give exactly one of `--from-step` and `--all-failed`.

```bash
# Restart from step `deploy`: it and every step that depends on it run again
stroem-api restart <job-id> --from-step deploy

# Restart every failed, cancelled or unfinished step and what depends on them
stroem-api restart <job-id> --all-failed

# Show the plan only; create no job
stroem-api restart <job-id> --all-failed --dry-run
```

The command prints the new job ID (or `Dry run: no job created`), the start steps, the steps that run again and the steps that are carried over. Carried-over failed steps are listed too.

### `rerun`

[Re-run](/guides/rerun-and-restart/#re-run) a top-level job: a new run of the same task with the input of the source job. `--input` and `--file` change fields; every other field takes the source job's stored value, secrets included.

```bash
# Run again with the same input
stroem-api rerun <job-id>

# Change one field
stroem-api rerun <job-id> --input '{"date": "2025-01-02"}'

# Give a new file to a file input field (repeatable)
stroem-api rerun <job-id> --file data_file=./report.csv
```

The command reads the source job, uploads the files for the re-run, and sends the names of the fields it does not set as `replay_fields`, so the server copies their stored values (the CLI never sends a masked value back). Limits: a field that the source job has but the task no longer declares gives `400 replay_fields names a field the task does not declare`; give the other fields with `--input` instead. A required field that the task gained since the source job ran is not checked: if you do not give it, the job is created without it (as for any re-run through the API). The MCP `rerun_job` tool checks both cases on the server.

### `approve` and `reject`

Decide a step in the `suspended` state: a `type: approval` gate, or an agent step that asked a question.

```bash
# Approve
stroem-api approve <job-id> <step>

# Approve with values for the approval's input fields
stroem-api approve <job-id> <step> --input '{"reason": "tested on staging"}'

# Answer an agent's question
stroem-api approve <job-id> <step> --input '{"response": "use the EU region"}'

# Reject; the reason becomes the step's error
stroem-api reject <job-id> <step> --reason "not before the release freeze"
```

`--input` must be a JSON object. You are recorded as the approver. Both commands need the Run permission on the job's task.

### `workspaces`

List all configured workspaces on the server: name, task / action / trigger counts (`(off)` when the server config disables the workspace's triggers), and the [display name and description](/guides/multi-workspace/#display-name-and-description) from the server config (`-` when unset).

```bash
stroem-api workspaces
```

### `triggers`

List triggers with their schedule, missed runs and last run (result, time and job ID). `hidden` means that the last run is a job you may not view; `never` means that the trigger has not run.

```bash
# Every workspace (a workspace that does not load is left out)
stroem-api triggers

# One workspace
stroem-api triggers --workspace data-team

# Only the triggers that need attention (the Dashboard card): missed runs or a failed last run
stroem-api triggers --attention
```
