---
title: Migrations 049 and 050 — git refs
description: Schema changes, rollout rules, and the behaviour that changes even for workflows that never use ref
---

Migrations `049_git_refs.sql` and `050_git_refs_indexes.sql` ship with
[git refs](/guides/git-refs/): `ref:` on flow-step actions, `type: task`
actions and scheduler/webhook triggers. This page covers the schema, the
rollout rules, and — most importantly — the behaviour that changes on
upgrade **even if no workflow uses `ref:`**.

## The migrations

**049** adds columns only, all nullable or with a constant default, so it is
instant on Postgres 11+ and rewrites no row:

| Table | Columns |
|---|---|
| `job` | `git_ref`, `task_folder` |
| `job_step` | `action_ref`, `task_workspace`, `task_ref`, `task_revision`, `pin_releases` (default `0`) |
| `task_state`, `workspace_state` | `git_ref` (`NULL` = the unpinned partition, i.e. every existing row) |

**050** adds four indexes under new names and drops the two state indexes
they replace:

- `idx_task_state_lookup_ref`, `idx_workspace_state_lookup_ref` (replace
  `idx_task_state_lookup` / `idx_workspace_state_lookup`);
- `idx_job_pinned_tasks` — a partial index on `job` for pinned jobs' ACL;
- `idx_job_step_pinned` — a partial index on `job_step` that lets every job
  read skip the redaction walk while no pinned row exists.

The server applies both at startup, each in one transaction. The two state
tables are usually small, but `idx_job_pinned_tasks` scans all of `job` and
`idx_job_step_pinned` all of `job_step`, each under a `SHARE` lock that
blocks writes to that table for the length of the scan. On a large
database, pre-run them `CONCURRENTLY` before deploying (the same approach as
migration 009). Run 049's `ALTER`s first, because the indexes need the
columns:

```sql
ALTER TABLE job ADD COLUMN IF NOT EXISTS git_ref TEXT;
ALTER TABLE job ADD COLUMN IF NOT EXISTS task_folder TEXT;
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS action_ref TEXT;
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS task_workspace TEXT;
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS task_ref TEXT;
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS task_revision TEXT;
ALTER TABLE job_step ADD COLUMN IF NOT EXISTS pin_releases INT NOT NULL DEFAULT 0;
ALTER TABLE task_state ADD COLUMN IF NOT EXISTS git_ref TEXT;
ALTER TABLE workspace_state ADD COLUMN IF NOT EXISTS git_ref TEXT;

CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_task_state_lookup_ref
  ON task_state (workspace, task_name, git_ref, created_at DESC, id DESC);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_workspace_state_lookup_ref
  ON workspace_state (workspace, git_ref, created_at DESC, id DESC);
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_job_pinned_tasks
  ON job (workspace, task_name, task_folder) WHERE git_ref IS NOT NULL;
CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_job_step_pinned
  ON job_step (job_id) WHERE action_ref IS NOT NULL OR task_ref IS NOT NULL;

DROP INDEX CONCURRENTLY IF EXISTS idx_task_state_lookup;
DROP INDEX CONCURRENTLY IF EXISTS idx_workspace_state_lookup;
```

Every statement in both migrations is `IF NOT EXISTS` / `IF EXISTS`, so they
become no-ops after the pre-run. The same text is in the header of
`050_git_refs_indexes.sql`.

Old server code ignores the new columns, so a mixed fleet keeps working as
long as no workflow uses `ref:` (next section).

## Rollout rules

- **Do not merge YAML that uses `ref:` until every server replica runs this
  release.** An older replica drops the unknown `ref:` key and silently runs
  the default branch instead. It also treats a step whose action is pinned
  in its own workspace as a live cross-workspace step.
- **Upgrade workers together with the server.** A new worker tells the server
  which job a state download is for (`?job_id=`) and retries a `503` on a
  workspace download. An old worker mounts the wrong state partition for a
  cross-workspace step or a pinned job (template rendering and uploads are
  server-side and stay correct), and fails a step whose files the server
  cannot fetch from git at that moment instead of retrying.
- **Give each server process its own `pin_store.dir`.** When at least one git
  workspace is configured, the server locks `{pin_store.dir}/.lock` at
  startup and refuses to start with `pin_store.dir … is in use by another
  process` if another process holds it. The default directory is
  `<temp>/stroem/pins`, so two servers on one host that share a temporary
  directory conflict; containers and pods each have their own. See
  [`pin_store`](/getting-started/configuration/#pin_store).
- **Budget disk for the pin store.** Each replica keeps a bare clone of every
  git workspace it serves an older revision or a ref of, plus one checkout
  per commit in use. The clones are never garbage-collected; with the default
  temporary directory they are rebuilt after a restart.
- **During the rolling deploy** an old leader's recovery sweep does not check
  that a running step is still the run it observed, so it can fail a step
  that a new replica just released. This needs pinned jobs, so it does not
  happen before YAML uses `ref:`.

## What changes without `ref:`

These apply to every deployment that upgrades, whether or not any workflow
uses `ref:`.

### More outlets mask secrets

Job detail always masked secret values; other places that show job content
did not. Now all of them use the same secret set:

| Where | Before | Now |
|---|---|---|
| Sync webhook response (`mode: sync`) | `output` returned raw | masked |
| Webhook job-status poll (`GET /hooks/{name}/jobs/{id}`) | `output` returned raw | masked, in every branch, with `Cache-Control: no-store` |
| MCP `get_job_status` | step `error_message` returned raw | masked |
| Worker detail (`GET /api/workers/{id}`), recent steps | `error_message` returned raw | masked |
| Job detail (`GET /api/jobs/{id}`) | `input`, `raw_input`, `output`, step `input` / `output` / `error_message` / `retry_history` | every string except identifiers: also `approval_message`, `approval_fields`, `when_condition`, `for_each_expr`, `action_image` |

A very short secret value can therefore mask more of the job page than
before.

These outlets can now also **fail closed**. If the secret set cannot be
completed — a pinned commit the job is connected to cannot be loaded right
now, or the database query that finds those commits fails — job detail
answers `503 {"error": "redaction set unavailable, retry"}`, the webhook
responses answer `503` with the same error plus `job_id`, MCP
`get_job_status` returns that error, and worker detail shows the affected
row's `error_message` as `••••••`. A deployment without any pinned job never
hits the commit case: while no pinned row exists, each read costs two index
probes (`idx_job_pinned_tasks`, `idx_job_step_pinned`) and no walk.

### Owner-side errors of cross-workspace actions are withheld at claim

A step whose action is another workspace's (`owner.action`) renders that
owner's input defaults and action body when a worker claims it. If that
rendering fails — say `{{ secret.TOKEN | json_encode | round }}` — the step
now fails with the fixed message `rendering action '<action>' of workspace
'<owner>' failed; details withheld`, in its `error_message`, the job log and
the claim response. The full, scrubbed error goes only to the server log.
Before, the error was shown after scrubbing known secret values, which
cannot catch every encoding a filter chain produces. The caller's own
`input:` errors and caller-supplied connection names stay visible. See
[Cross-Workspace References](/guides/cross-workspace-references/#trust-model-for-owner-side-rendering-errors).

Visible claim-time errors in input preparation gain one link in their chain,
`resolving Caller inputs:` or `resolving ActionDefault inputs:`.

### Webhooks are re-authenticated after `force_refresh`

A webhook with `force_refresh: true` used to authenticate the caller, reload
the workspace, and then create the job from the definition captured
**before** the reload. Now:

1. The caller is authenticated against the loaded definition first, so an
   unauthenticated caller still cannot trigger a git fetch.
2. After the reload, the webhook is matched again: a webhook the refresh
   removed answers `404`; a workspace that is unavailable after the reload
   answers `500`.
3. The caller is authenticated again against the refreshed secret: after a
   rotation, the old secret gets `401`. The new secret is accepted only once
   the server has loaded it (its watcher poll, or a previous refresh).
4. The job is created from the refreshed definition, so a changed `task`
   takes effect at once.

### Webhook error codes

Webhook target errors are now classified like the execute API: an unknown
task or workspace (and, with refs, a bad or missing ref) answers `400`
instead of `500`. A transient problem still answers `500`. A trigger's
`task` may now also name another workspace's task (`ws.task`), and the job
is created in that workspace — for webhooks and scheduler triggers alike. A
scheduler fire whose target cannot be resolved is logged as
`Trigger '<ws>/<name>' MISSED: …` and has no other effect.

### Task state of cross-workspace steps

A step that runs another workspace's action used to read task state from
`(action owner, caller's task name)` — usually an empty partition, or the
state of an unrelated same-named task in the owner — and its upload was
rejected. Now every step reads and writes the state of its own **job**: the
job's workspace and task. The server takes these coordinates from the job;
the workspace and task in the worker's state paths are no longer used (an
old worker that sends no `job_id` on downloads still gets the path's
coordinates).

### Old revisions of git workspaces are served

A worker downloads each step's files at the job's revision. For a git
workspace, a revision that is not the current one — the workspace moved on
since the job was created, or its default branch currently fails to load —
used to be served only if it happened to be in the replica's tarball cache,
and was otherwise a `404` that failed the step. Now it is built from a clean
checkout of that commit in the replica's pin store:

- the tarball has **no `.git` directory**. A script that runs `git` in the
  workspace directory for a job whose revision is no longer current sees no
  repository; use `{{ job.revision }}` instead;
- the first such request on a replica clones the workspace's repository into
  `pin_store.dir`;
- while the git server is unreachable and the commit is not yet on that
  replica, the server answers `503` with `Retry-After: 5`, and a new worker
  retries for about a minute.

The current revision of a healthy workspace, and folder workspaces, are
served exactly as before.

### MCP `list_jobs` fills its pages

With ACL configured, `list_jobs` used to fetch `limit` jobs and then drop
the ones the user may not see, so a page could come back short or empty
while older permitted jobs existed. It now filters in the database before
`limit`, like `GET /api/jobs`.

### A claim-time or recovery failure no longer overwrites a finished step

A claim-time render failure, and the recovery sweep's stale-worker and
step-timeout phases, now fail a step only while it is still `running` under
the claim they observed. A step that completed (or was claimed again) in
between is left alone.

### Smaller changes

- New API fields: `ref` on jobs (job list, job detail, MCP `get_job_status`
  and `list_jobs`); `action_ref`, `task_workspace`, `task_ref`,
  `task_revision` on steps; `ref` and `revision` on `child_jobs[]` entries.
  All are `null` for jobs that use no ref.
- New template variables `{{ job.ref }}` and `hook.ref`, empty or `null`
  without a ref.
- The server-log warning for a SOPS file that cannot be decrypted now reads
  `Skipping SOPS file '<path>': <error>` (was `Skipping '<path>': failed to
  read file: <error>`).
- New metrics `stroem_pin_loads_total` and `stroem_pins_cached` (see
  [Metrics](/operations/metrics/#pin-store)); the gauge is `0` for every git
  workspace until a commit is pinned.

## Once you use `ref:`

- Task duration statistics (percentiles, ETA) leave out pinned jobs; they
  describe the live task only.
- A pinned job is authorised by its own commit's task folder everywhere,
  including lists (see [Authorization](/operations/authorization/#jobs-and-task-folders)).
- Re-run and Restart of a pinned job re-resolve its ref; the restart preview
  then contacts the git server and can answer `500` during an outage.
- Pinned commits load lazily: the first pinned run after a push or a server
  restart pays for a fetch, a checkout and a config load.
- There is no allow-list: anyone who can merge a `ref:` into any workspace
  can run any ref of any configured git workspace with that workspace's
  secrets. See [Git Refs → Security](/guides/git-refs/#security).
