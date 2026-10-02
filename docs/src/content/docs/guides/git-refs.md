---
title: Git Refs
description: Run an action or task from a specific branch, tag or commit of a git workspace
---

A reference to an action or a task can carry a `ref:` — a **branch**, a
**tag** or a full **commit SHA** of the git workspace that owns it. The
target then runs from that commit: its YAML (flow, inputs, action
definitions, hooks, secrets, connections) and its files. The typical use is
several releases of one workspace running side by side, with the default
branch as the manifest that pins which release each step, task or trigger
runs.

```yaml
# on main
tasks:
  daily:
    flow:
      import:
        action: import            # own workspace @ release/2.3
        ref: release/2.3
      export:
        action: billing.export    # workspace `billing` @ tag v4.1.0
        ref: v4.1.0

actions:
  nightly-2-3:
    type: task
    task: nightly                 # own task @ commit
    ref: 3f2a9c0e1b2c3d4e5f60718293a4b5c6d7e8f901

triggers:
  nightly-billing:
    type: scheduler
    cron: "0 2 * * *"
    task: billing.nightly         # a trigger may name another workspace's task
    ref: v4.1.0
```

Cutting a release is a pull request to `main` that bumps a `ref:`. Cutting
`release/2.5` needs no server configuration change.

:::caution[Before you merge YAML that uses `ref:`]
Every server replica must run a release with this feature first. An older
replica drops the unknown `ref:` key and silently runs the default branch.
See [Upgrading](#upgrading).
:::

## Where `ref` is allowed

| Place | `ref` qualifies |
|---|---|
| Flow step `action:` | the action |
| `type: task` action `task:` | the task |
| Scheduler and webhook trigger `task:` | the task |

`ref:` is **rejected**, never silently ignored, on:

| Place | `stroem validate` | At runtime |
|---|---|---|
| A hook (`on_success`, `on_error`, `on_cancel`, `on_suspended`), or a hook whose action is a `type: task` action carrying `ref` | error: "`ref` is not supported on hooks yet" | the hook is not fired; the error is logged to the job that fired it |
| An event-source trigger | error: "`ref` is not supported on event_source triggers yet" | the consumer is not started; a warning is logged on every reconcile |
| An agent `tools: [{task: …}]` entry | error: "`ref` is not supported on agent task tools yet" | the tool call is rejected with `400` |
| A flow step whose ref'd action is `type: agent` | — | `400`: "action '…': agent actions cannot be referenced with `ref` yet" |

`ref:` on an action of any type other than `task` is a `stroem validate`
error. The server does not validate workspace YAML when it loads it, so
there such a `ref:` has no effect — run `stroem validate` in CI.

A ref is a fixed string. A value containing `{{` or `{%` is rejected: refs
are never templated.

## Ref forms

| Written | Meaning |
|---|---|
| 40 hex characters | that commit (normalised to lowercase) |
| `refs/heads/<name>` | branch `<name>` |
| `refs/tags/<name>` | tag `<name>` (an annotated tag resolves to its commit) |
| any other `<name>` | branch `<name>` if it exists, else tag `<name>` |

A bare name must be a valid git branch name. Other `refs/…` namespaces
(`refs/pull/…`, `refs/notes/…`) are not supported. Short SHAs are not
supported either: a 4–39 character hex string that names no branch or tag
fails with "short commit SHAs are not supported; use the full 40-character
SHA".

## How a reference resolves

The **owner** of the referenced name is decided first. Then the name is
looked up in the owner's config **at the ref**:

| Written | Resolves to |
|---|---|
| unqualified name + `ref` | your own workspace at the ref |
| `ws.name` + `ref` | workspace `ws` at the ref |
| unqualified name, no `ref`, inside a pinned config | the same commit (the pin is inherited) |
| `ws.name`, no `ref`, inside a pinned config | `ws`'s live config (the [cross-workspace](/guides/cross-workspace-references/) rule) |
| a library item + `ref` | `400` — libraries are server-level and have no ref |
| `ref` on a folder workspace | `400` — only git workspaces have refs |
| `ws.name` + `ref`, `ws` not configured | `400` — unknown workspace |

With `ref`, the owner is decided from the name alone, so the name does not
have to exist on your default branch: a step may call an action that exists
only on the release branch. Inside a release, an unqualified name resolves
inside that release: `nightly` at `release/2.3` calling `action: helper`
gets 2.3's `helper`.

The owner does not have to be healthy. A ref can be resolved while the
owner's default branch fails to load, because refs use their own copy of the
repository (see [Storage](#storage-on-the-server)).

A ref'd target runs **in the owner's context at that commit**: its files,
its `secrets:`, its connections. Access control, the `shared: true`
connection gate and error withholding key on the **workspace name**, never
the ref. `own@release/2.3` is your own workspace; `billing@v4.1.0` is
cross-workspace, exactly like `billing`.

A task may call **itself** at another ref: the same task at another commit
is a different version. Nesting is bounded by the maximum task depth (10),
as for any other `type: task` chain.

## Pinning

Every ref in a job resolves to a commit **once**, when that job is created:

- a flow step's action, and a `type: task` step's task, are pinned when the
  job containing them is created;
- a trigger resolves its ref when it fires;
- a child job resolves its own nested refs when it is created.

A branch that moves mid-job therefore cannot split one job across two
commits of the same ref.

A job that runs on a ref is a **pinned job**: a ref'd `type: task` child, a
job created by a trigger with `ref:`, or a job derived from a pinned job
(below). Its flow, inputs, hooks, retries and
approvals all come from that one commit for its whole life, even if the
branch moves meanwhile.

| Derived job | Runs at |
|---|---|
| `type: task` child with its own `ref` | that ref, resolved when the parent was created |
| `type: task` child without `ref`, whose task is in the same workspace as the pinned config its action is written in | that config's commit (inherited) |
| `type: task` child without `ref`, whose task is in another workspace (or whose action is a live foreign action) | that workspace's live config |
| Hook job of a pinned job | the pinned job's ref and commit; the hook comes from that commit |
| Task-level retry of a pinned job | the same ref and commit |
| Agent task-tool child of a pinned job | the same ref and commit |
| Re-run or Restart of a pinned job | the ref **re-resolved** — a branch moves to its current tip (see [Re-run and Restart](#re-run-and-restart)) |

In templates, `{{ job.revision }}` of a pinned job is its commit and
`{{ job.ref }}` the ref as written. `{{ job.ref }}` renders as an empty
string for every other job. Hooks get the same value as `hook.ref`. Over the
API, a pinned job carries `ref`, and steps carry `action_ref`,
`task_workspace`, `task_ref` and `task_revision` (see the
[API reference](/reference/api/#get-job-detail)). The UI shows a
`@ release/2.3 · 3f2a9c0` badge on the job and on every step that carries a
pin.

## Freshness

Branches and tags are listed with one `ls-remote` per workspace. The listing
is cached for the owner workspace's `poll_interval_secs`, the same freshness
as the default branch.

| Situation | What happens |
|---|---|
| Push to `release/2.3`, next run 2 minutes later | the new commit |
| Push, next run 20 seconds later | may still get the previous commit |
| A branch or tag that was not in the cached listing | the listing is refreshed at once, so a new branch is found immediately |
| A job is running when the push lands | it keeps its commit |
| The branch is deleted | resolution fails once the cached listing expires (`400` / trigger MISSED) |
| The git server is unreachable | the last listing is used, with a warning; with none, the run fails (`500` / trigger MISSED) |

Each server replica keeps its own listing, so two jobs created on different
replicas within one `poll_interval_secs` window may resolve a branch to
different commits.

Nothing is loaded ahead of time. The first run after a push, and the first
run after every server restart, fetches, checks out and loads that commit.
That can take seconds for a large repository.

## Running pinned steps

### Claim

A worker's claim can land on a replica that has not loaded the step's commit
yet (another replica created the job, or the server restarted). The claim
then loads the commit itself, for at most
[`pin_store.claim_load_budget_secs`](/getting-started/configuration/#pin_store)
(default 20 seconds). Keep that budget below the workers'
`request_timeout_secs` (30 seconds by default).

If the load does not finish within the budget, or the git server is
unreachable, the claim is **released**: the step goes back to `ready` and is
offered again 10 seconds later. The load continues in the background, so the
next claim usually finds the commit ready. The job log shows
`[pin] etl@release/2.3 (3f2a9c0) not available yet on this server, retrying: …`.
A release is not a failure: it uses none of the step's retry attempts.
After 30 releases the step fails with
`[pin] … still unavailable after 30 attempts: …`, and step retry applies as
usual. Each release cycle takes up to the claim budget, plus the 10-second
release delay, plus the time until a worker polls again — 30 × (budget + 10 s
+ poll). With the defaults that is a little over 5 minutes when the git
server fails fast, and about 15 minutes or more when it hangs until the
budget runs out.

A permanent problem (the commit was force-pushed away, or its YAML does not
load) fails the step at once with `[pin] … cannot be loaded: …`.

### Files

The worker downloads the step's files at the pinned commit. The server
builds that tarball from a clean checkout of the commit, which has no `.git`
directory. If the server cannot reach the git server for a commit it has not
loaded, it answers `503` with `Retry-After: 5`, and the worker tries again
every 5 seconds — up to 12 attempts in all, about a minute — before it fails
the step.

### Settlement

Moving a pinned job forward after a step (promoting the next steps, firing
hooks, scheduling a retry) needs the commit's definitions. If they cannot be
loaded at that moment, the job waits, with
`[pin] … not available yet: …` in its log. The recovery sweep re-advances
such a job on every pass (every `recovery.sweep_interval_secs`, 60 seconds
by default), so it continues once the git server is reachable again. That
includes a job that never started because its step failed at claim (after
30 releases) or its files could not be downloaded: once the definitions
load, the sweep skips the steps that depended on it and settles the job.

If the commit can never load again (force-pushed away, or the YAML at it is
broken), the job is **failed** with `[pin] … cannot be loaded: …`, and its
steps that have not started yet are cancelled. Steps already running keep
running — their workers are not told to stop — and finish on their own. Its
hooks and task-level retry do not run, because their definitions are
unreadable.

## Task state

A pinned job's [task state](/guides/task-state/) and global state are kept
**per ref string**: `release/2.3` and `release/2.4` never overwrite each
other, nor the default branch's state. Bumping `ref: v2.3.1` → `ref: v2.3.2`
starts with empty state. Manual state uploads always write the default
(unpinned) partition.

## Access control

A pinned job is authorised by the folder its **own commit** declares for its
task (`task_folder` on the job), never by the live task's folder, even when
a task of the same name exists on the default branch. Two refs of one task
that declare different folders are therefore authorised independently, in
job detail, logs, artifacts, the log stream, worker detail, the job list
and over MCP. See [Authorization](/operations/authorization/#jobs-and-task-folders).

Task duration statistics (percentiles, ETA) describe the live task only:
pinned jobs are left out.

## Secrets and redaction

A commit's secrets can differ from the default branch's, and can exist only
at that commit. The API responses that show a job's input, output and step
errors mask secrets with the live workspaces' values **plus** the secret
values of every pinned commit the job is connected to:

- the commits of the job and its steps;
- the commits of every job in the same job tree (parent, children,
  siblings), because values are copied between them;
- the commits of the jobs it was made from — the job that fired a hook, the
  job a restart was restarted from, the job a re-run re-ran (a re-run of a
  task retry replays that retry's resolved input), the job a task retry
  retries — and of their job trees. Chains of restarts and re-runs are
  followed up to 32 links; a longer chain masks everything.

These responses are job detail (`GET /api/jobs/{id}`), the recent steps on
worker detail, the sync webhook response and the webhook job-status poll,
and MCP `get_job_status`.

Job **logs** (REST, the WebSocket stream, MCP `get_job_logs`) and
**artifacts** are not masked at all: a secret that exists only at a ref and
that a script prints appears in the log exactly as printed. See
[Secrets & Encryption](/guides/secrets/#api-redaction).

When some of the secret values those responses need cannot be loaded, the
responses never fall back to a partial mask:

| Situation | Job detail, webhook, MCP | Worker detail |
|---|---|---|
| A commit cannot be loaded right now (git server unreachable on this replica) | `503` "redaction set unavailable, retry" (webhook responses keep `job_id`; MCP returns an error) | that row's `error_message` is `••••••` |
| A commit can never load (force-pushed away, broken YAML, secrets that have not loaded for an hour), or the job is connected to too many jobs to check | `200`, with every content string masked as `••••••`; ids, statuses and timestamps stay | that row's `error_message` is `••••••` |

The price is that a cold commit anywhere in a job's tree can make an
unrelated job's detail answer `503` during a git outage.

Errors are handled the same way as for
[cross-workspace references](/guides/cross-workspace-references/#trust-model-for-owner-side-rendering-errors):
an error rendering the **owner's** templates of a ref'd action or task in
another workspace is withheld from the caller, with a fixed "details
withheld" message, and the full error goes to the server log. A commit whose
YAML does not load is reported only as
`[pin] ws@ref (sha) cannot be loaded: its configuration does not load`,
because the loader's message can quote secret values.

### Secrets that no longer decrypt

A commit's `sops` files and `vals` references are read when the server
loads that commit. If they fail — the `sops` key or the KMS key is not
available, a `vals` backend is down, a `vals` path no longer exists — the
commit counts as **not available yet**, like a git outage: claims are
released and retried, a job waits, the responses above answer `503`. The
server remembers such a failure for 30 seconds, so `sops` and `vals` run at
most twice a minute per commit, not on every request.

Old commits often stay undecryptable for good: key rotation re-encrypts only
new commits, and a retired KMS key or a deleted `vals` path does not come
back. So once a commit's secrets have failed for **one hour** on a server
(counted from the first failure that server saw, and reset by any
successful load of that commit), the server treats the commit as one that
can never load: claims fail the step, a waiting job fails, and the
responses above answer `200` with everything masked. The server log names
the cause (`its secrets have failed to load for …s …; treated as
permanent`). Each replica counts on its own, and a restart starts over.

## Errors

| Condition | Execute API, webhook | Scheduler trigger | When a step runs |
|---|---|---|---|
| Unknown workspace, folder owner, library item + `ref`, invalid or templated ref | `400` | MISSED | — (the job is never created) |
| Branch or tag not found | `400` | MISSED | — (resolved at creation) |
| Name missing at that commit ("has no action … at ref …" / "has no task … at ref …") | `400` | MISSED | — (checked at creation) |
| The YAML at that commit does not load | `400`, fixed message | MISSED | step fails, fixed message |
| Commit no longer exists (force-pushed away) | `400` | MISSED | step fails; a pinned job fails |
| Git server unreachable with nothing cached, load timeout, `sops`/`vals` failure | `500` | MISSED | claim released and retried, up to 30 times (see [Claim](#claim)) |
| The commit's secrets (`sops`/`vals`) have failed to load for an hour on that server | `400`, fixed message | MISSED | step fails, fixed message; a pinned job fails |
| `ref` on an agent action | `400` | MISSED | — |

A `type: task` step whose pinned action or task cannot be loaded when it is
dispatched fails, even when the cause is transient: dispatch has no
release-and-retry path. This needs both a replica that has never loaded the
commit and a git outage.

A trigger fire that cannot resolve its target is logged as
`Trigger '<ws>/<name>' MISSED: …` and has no other effect: no
`cancel_previous`, no skipped row. It is not replayed.

## Triggers

A scheduler or webhook trigger's `task` may name another workspace's task
(`ws.task`), with or without `ref`. The job is created in the task's owner
workspace, at the ref's commit when there is one. The ref is resolved when
the trigger fires, before the concurrency policy. The concurrency policy is
keyed on the **defining** workspace's trigger (`source_id` =
`<defining ws>/<trigger>`), and a `skip` row is recorded in the task's
owner workspace with the resolved ref and commit. `triggers: false` on the
defining workspace still suppresses the trigger.

A webhook with `force_refresh: true` reloads its workspace and is then
matched and authenticated again against the refreshed definition, so a
refresh that changes the `ref` runs the new one (see
[Webhook API](/reference/webhook-api/)).

## Re-run and Restart

Re-running or restarting a pinned job re-resolves its ref **before** the
task is looked up: a branch moves to its current tip, a tag or SHA stays
put. The task is looked up at that commit, so it may exist only on that
branch, and the new job is pinned to the new commit with the same ref.
Carried-over steps of a restart keep the output they produced at the old
commit; the restarted steps run at the new one.

| Situation | Answer |
|---|---|
| No access to the source job's folder, or to the folder the task declares at the re-resolved commit | `404` |
| `View` only on either folder (both need `Run` on both) | `403` |
| Re-run posted to a different task name than the source's | `400` "Source job … is a run of task '…', not '…'" |
| The task no longer exists at the re-resolved commit | `400` "Task '…' does not exist at ref '…' (sha)" |
| The ref no longer exists | `400` |
| Git server unreachable | `500` |

Access is checked twice: on the source job's folder (its `task_folder`)
first, then on the folder the task declares at the re-resolved commit — the
folder the new job will carry. Moving a task into a stricter folder on its
branch therefore also restricts who can re-run or restart its older jobs. A
restart dry run of a pinned job also contacts the git server, so the preview
can answer `500` during an outage. The UI hides Re-run and Restart for a pinned
job whose task does not exist on the default branch; use the API for those
(`POST /api/workspaces/{ws}/tasks/{task}/execute` with `source_job_id`, or
`POST /api/jobs/{id}/restart`).

## Storage on the server

Each server replica keeps its own **pin store**: one bare clone per git
workspace, never the clone the default branch is loaded from, plus one
read-only checkout and one loaded config per commit in use. Commits that no
active job needs are evicted, beyond the `keep_recent_per_workspace` most
recently used ones. The bare clones are never garbage-collected. See
[`pin_store`](/getting-started/configuration/#pin_store) for the settings,
and [Metrics](/operations/metrics/#pin-store) for `stroem_pin_loads_total`
and `stroem_pins_cached`.

Loading a commit uses the
[`workspace_reload`](/getting-started/configuration/#workspace_reload)
budgets: `peek_timeout_secs` for the branch and tag listing, and
`load_timeout_secs` for fetch, checkout and config load together. At most
four commits load at once per replica, separately from the default-branch
watchers.

## Security

There is no allow-list. Anyone who can merge a `ref:` into **any** workspace
can make the server run **any** branch, tag or commit of **any** configured
git workspace with that workspace's secrets. That includes resolving
whatever `vals` references an unreviewed branch contains. Restrict who can
merge to your workspaces' default branches accordingly.

## Upgrading

**Do not merge YAML that uses `ref:` until every server replica runs a
release with this feature.** An older replica ignores the unknown `ref:` key
and silently runs the default branch instead.

The same release changes some behaviour for **every** job, including jobs
that use no `ref:` at all: redaction of webhook, MCP and worker-detail
output, webhook re-authentication after `force_refresh`, task state of
cross-workspace steps, and how old revisions' files are served. Read
[Migrations 049 and 050](/operations/migration-049-050/) before upgrading.

## Not yet supported

- A per-run ref override (testing a feature branch from the UI, API, MCP or
  CLI).
- `ref` on hooks, event sources, agent actions and agent task tools.
- Libraries at a ref.
- Manual state upload to a ref's partition.
- Re-run and Restart from the UI of a pinned job whose task does not exist
  on the default branch (the API works).
