---
title: Cross-Workspace References
description: Reference another workspace's action directly from a flow step, with no library setup required
---

A flow step's `action:` can point at another workspace's action directly — `owner_workspace.action_name` — with no server configuration and no library. The step then runs with that **owner** workspace's files, secrets, and connections; the caller only supplies plain input values.

This is separate from [Libraries](/guides/libraries/): a library is an explicit, admin-configured shared source merged into every workspace. A cross-workspace reference is a direct, ad-hoc pointer from one workspace to another, resolved live at job-creation time.

## Syntax

```yaml
tasks:
  daily:
    flow:
      recalc:
        action: jobs.recalc-agg-sessions   # owner workspace: "jobs", action: "recalc-agg-sessions"
        input:
          dataset: "{{ run.output.dataset }}"
```

`jobs.recalc-agg-sessions` is resolved against the `jobs` workspace's configuration — no `libraries:` entry, no import step. The action must simply exist in that workspace.

## No library needed

Libraries require a `libraries:` entry in `server-config.yaml` (Git repo or folder) that gets imported and prefixed into every workspace at load time. Cross-workspace references need none of that — any workspace can reference any other workspace's action by name, as long as both workspaces are configured on the same server.

## Precedence: library first, then workspace

A dotted name is resolved in this order:

1. **Library item.** Libraries are flattened into each workspace's config at load time under the `libname.` prefix, so `foo.bar` is already a literal key in `workspace_config.actions` if `foo` is a configured library. This is checked first and is unchanged from existing behavior.
2. **Cross-workspace reference.** If there's no local match, the name is split on the first `.` into `(workspace, item)`, and `item` is looked up in that workspace's config.

Unqualified (non-dotted) names are always local — this is fully backward-compatible; existing workspaces and library references keep working exactly as before.

## Owner-context execution

The key rule: **a cross-workspace action runs in the workspace that owns it.**

- The flow step's `input:` map still renders in the **caller's** context — `{{ run.output... }}`, `{{ input... }}`, and previous-step outputs all resolve against the calling job, exactly as for a local step.
- The **action body** — its script/command/env templates, connection-typed inputs, and any `{{ secret.* }}` references — renders against the **owner** workspace's files, secrets, and connections.

This means the caller passes only plain input values; it does not need any of the owner's connections, connection types, or secrets configured locally. A connection-typed input on the cross-workspace action (for example, an input defaulting to `clickhouse-prod`) resolves by bare name in the owner workspace, not the caller's.

### Revision pinning

The owner workspace's revision is pinned when the job is created (the same discipline as `job.revision`), so a mid-run change to the owner workspace's config cannot shift the action under an in-flight job. The worker fetches the owner workspace's tarball at that pinned revision for the step — a cross-workspace step downloads exactly one workspace tarball (its owner), never two.

## Connections

A connection-typed input may name another workspace's connection directly,
and a type may be another workspace's type. Both use the same `workspace.name`
addressing and are independent of each other:

```yaml
# ai_traffic_model — no ClickHouse type, connection, or secret defined here
tasks:
  daily:
    input:
      clickhouse:
        type: jobs.clickhouse          # the TYPE lives in workspace `jobs`
        default: jobs.clickhouse-prod  # the CONNECTION lives in `jobs` too
    flow:
      run:
        action: score-all              # a LOCAL action
        input:
          clickhouse: "{{ input.clickhouse }}"
```

### `shared: true`

A connection can be referenced from another workspace **only** if its owner
marks it shared:

```yaml
# jobs workspace
connections:
  clickhouse-prod:
    type: clickhouse
    shared: true
    host: ch.internal
    password: "{{ secret.ch_password }}"
```

Unshared connections are private to their workspace. Whether — and when — a
reference to one from elsewhere is rejected depends on where the reference
appears:

- **Literal references** — task input defaults, and a flow-step value written
  as a plain string — are checked at job creation and fail immediately with
  `400 Bad Request` and a message ending in `is not shared`.
- **Templated flow-step values** (anything containing `{{ ... }}`) can't be
  checked before the job runs, since the template may resolve to a different
  connection depending on prior step output. These are resolved when the step
  is claimed and fail that step instead — the job is created successfully,
  and the reason ("... exists but is not shared") appears in the step's error
  message.

Inside its own workspace the flag is ignored.

### How types match

Every type reference is normalised to `(workspace, type)`. A bare name means
the workspace the YAML is written in, so `type: clickhouse` in `ai_traffic_model`
is a *different* type from `type: clickhouse` in `jobs`, even if both exist.
A connection satisfies an input only when both resolve to the same pair.
Consequences:

| Input declares          | Connection value       | Connection's own `type:` | Result |
|--------------------------|------------------------|--------------------------|--------|
| `jobs.clickhouse`       | `jobs.clickhouse-prod` | `clickhouse` (in `jobs`) | OK if shared |
| `jobs.clickhouse`       | `infra.ch-eu`          | `jobs.clickhouse`        | OK if shared — type and connection in different workspaces |
| `clickhouse` (local)    | `jobs.clickhouse-prod` | `clickhouse` (in `jobs`) | Rejected: `caller.clickhouse ≠ jobs.clickhouse` |
| `jobs.clickhouse`       | `local-ch` (local)     | `jobs.clickhouse`        | OK — a local connection may adopt a foreign type |

Untyped connections (no `type:`) match any input type, as they do locally.

A connection whose `type:` is in another workspace gets that type's property
defaults applied and its required fields checked when the job is created (the
owner's load-time validation never saw the type).

### Cross-workspace actions and the `shared` gate

On a step whose action is `owner.action`, connection names the **caller**
supplies resolve in the caller first and then, if not found, in the owner —
but only shared ones. Names that come from the owner action's own `default:`
resolve in the owner without the gate. Before this release the caller could
name any owner connection bare; now the owner must mark it `shared: true` or
the step fails with `... exists but is not shared`. Owner-side defaults are
also never re-rendered against a caller-supplied value — only fields the
caller actually left out are filled in and rendered — so a caller cannot
smuggle out the owner's secrets through a value it supplies itself.

### Dropdown

The task form lists every eligible connection: local ones by bare name, then
shared foreign ones as `workspace.name`.

### Redaction and visibility

Resolved connection values are stored with the job. Job detail redacts every
workspace's `secrets` values and every connection property whose type marks it
`secret: true`. Everything else in a shared connection is visible to anyone
with View permission on a task that uses it, in any workspace — mark
credentials `secret: true` in the connection type.

- Redaction is computed from the workspaces currently loaded on the server,
  not from what was loaded when the job ran. If an owner workspace becomes
  unhealthy (fails to reload, Git source unreachable, etc.), its secret-marked
  values are **not** masked in already-created jobs until that workspace loads
  successfully again.

### Offline CLI

`stroem validate` warns on qualified type references (they are validated at job
creation). `stroem run` cannot resolve a qualified connection and fails with
`cross-workspace connection references require a server`.

## Open access

Any workspace may reference any other workspace's actions — there is no ACL gate on cross-workspace action references in this release. This is an intentional, low-friction choice for deployments where all workspaces are internal and already gated at the task-execution boundary.

Connection references are narrower: a foreign connection is only reachable when its owner opts in with `shared: true` (see [Connections](#connections) above). There's no workspace-level `exports:` allowlist — the flag is per-connection and global to the server, not scoped to specific callers.

## Before / after example

This mirrors the incident that motivated this feature: `ai_traffic_model`'s `daily` task needed to call `jobs.recalc-agg-sessions` and use the `jobs` workspace's `clickhouse-prod` connection, without either being configured locally.

The `jobs` workspace side is unchanged between "before" and "after" except for one flag:

```yaml
# jobs workspace
connections:
  clickhouse-prod:
    type: clickhouse
    shared: true                      # required for any other workspace to name this connection
    host: ch.internal
    password: "{{ secret.ch_password }}"
```

**Before** (fails — no `jobs` library, no local `clickhouse-prod` connection):

```yaml
# ai_traffic_model / daily.yaml
tasks:
  daily:
    input:
      day: { type: date, required: false }
      clickhouse: { type: clickhouse, required: true }   # local connection-type + connection needed
    flow:
      run:
        action: score-all
      recalc:
        depends_on: [run]
        action: jobs.recalc-agg-sessions                  # no "jobs" library configured -> unresolved
        for_each: "{{ run.output.periods | json_encode() }}"
        input:
          clickhouse: "{{ input.clickhouse }}"
          dataset: "{{ run.output.dataset }}"
          date: "{{ each.item.date }}"
          date_to: "{{ each.item.date_to }}"
          company_nums: "{{ run.output.companies }}"
```

**After** (works — cross-workspace reference, no local ClickHouse config at all):

```yaml
# ai_traffic_model / daily.yaml — no ClickHouse connection/type/secret defined locally
tasks:
  daily:
    input:
      day: { type: date, required: false }        # note: clickhouse input REMOVED
    flow:
      run:
        action: score-all
      recalc:
        depends_on: [run]
        action: jobs.recalc-agg-sessions           # cross-workspace action (owner: jobs)
        for_each: "{{ run.output.periods | json_encode() }}"
        input:
          clickhouse: "clickhouse-prod"            # resolved in jobs (owner) context
          dataset: "{{ run.output.dataset }}"
          date: "{{ each.item.date }}"
          date_to: "{{ each.item.date_to }}"
          company_nums: "{{ run.output.companies }}"
```

- Job creation succeeds: `"clickhouse-prod"` is a caller-supplied value on a cross-workspace action's input, so it is looked up in the caller (`ai_traffic_model`) first, misses (no such local connection), and falls back to the owner (`jobs`) — where it exists **and** is marked `shared: true`, so the fallback is allowed.
- The `recalc` step is stamped with the owner workspace (`jobs`) and its pinned revision.
- The worker fetches the `jobs` tarball for that step, so files like `agg_sessions_4.sql` are present, and `clickhouse-prod` resolves using `jobs`' own secrets.

## Calling tasks in other workspaces

A `type: task` action can name a task in another workspace, two ways:

```yaml
# Form A — direct
actions:
  call-deploy:
    type: task
    task: platform.deploy        # workspace "platform", task "deploy"

# Form B — through the owner's own wrapper action
tasks:
  release:
    flow:
      deploy:
        action: platform.run-deploy   # platform defines run-deploy as type: task
```

Three workspaces can be involved: the **caller** (where the flow step
lives), the **action owner** (the workspace that owns the `type: task`
action — the caller for form A, `platform` for form B), and the **task
owner**. The `task:` name is looked up in the action owner first —
library-flattened names like `common.deploy` live there — and only on a
miss is it split into `workspace.task`.

The child is a real job of the task owner: it runs with that workspace's
files, secrets, connections and revision, appears in its own job list,
fires that task's own `on_*` hooks, and is governed by that workspace's
ACL. The caller's step receives the child's status and output exactly as
for a local child.

### Inputs

- The flow step's `input:` renders in the **caller's** context.
- The `type: task` action's own `input` defaults are read from the
  definition persisted on the step at job creation (so editing the action
  does not change an in-flight job), rendered with the action owner's
  live secrets.
- Connection-typed fields of the **task's** input schema resolve by where
  the value came from: a caller value resolves in the caller first, then
  in the task owner if the connection is `shared: true`; an action default
  resolves in the action owner first, then the task owner if shared; the
  task's own defaults resolve in the task owner without a gate.
- A value that crosses a workspace boundary into a connection-typed field
  must be a connection name — an object, array, number, boolean, or null
  is rejected. This is a `400` at submit only for a literal value supplied
  by the caller's flow step, on a step with no `when` guard, when the
  caller isn't the task's own workspace; a templated value, a value on a
  `when`-guarded step, or a foreign action default instead fails the step
  at dispatch, if the step is reached. Within one workspace an object still
  passes through as before.
- The **parent** job's own step `input` (what `GET /api/jobs/{id}` shows
  for the `type: task` step itself, not the child) only ever shows what
  the caller supplied — never the action owner's resolved defaults — when
  the owner is a different workspace from the caller. This keeps the
  owner's unshared connections, resolved into plain values by its
  defaults, out of view for anyone who can only see the caller's job. The
  child job's own input is unaffected and still carries the full merged
  input.

### Revision

A same-workspace child inherits its parent's revision. A cross-workspace
child gets the task owner's **current** revision at the moment it is
created — the value the owner's own triggers would use — unlike
cross-workspace *actions*, whose owner revision is pinned when the parent
job is created.

### Trust model

**Calling a task is delegation to its author.** Any workspace can call any
task; the `Run` permission on the caller's task is the authorization, and
there is no per-child check. Write a task other workspaces may call as you
would a webhook-triggered task: any input the schema accepts may arrive.
The connection rules above cover connection-typed inputs *declared on the
task* only — a plain input that your own task templates into a connection
name on one of its actions binds in your own workspace, with no `shared`
gate, exactly as when your own trigger runs it. The child's output is
returned to the caller verbatim, so a task that emits credentials in its
output discloses them to every caller.

A child is not a scheduler run of the owner's task: it takes no
task-level retry, fires no workspace-level hooks, and is cancelled when the
caller's job is cancelled (see the cancellation caveat below).

The child's own job detail, logs, and artifacts are governed by the task
owner's ACL, independently of the caller's. A caller without `View` on the
task owner's task gets a `404` on the child's detail even though its own
job succeeded; an admin, or anyone with `View` on the owner side, can open
it.

A chain of `type: task` calls across workspaces is bounded by the same
maximum task depth (10) as any indirect recursion within one workspace.
Ancestry-based cycle detection is deliberately not added: bounded indirect
recursion guarded by a `when` condition is a legitimate pattern it would
break.

### The parent → child link

The job detail's step view lists every child job ever created for a
`type: task` step, newest first. This is **execution history, not attempt
identity** — a step retry resets the row before its replacement child
exists, and a duplicate dispatch (see the caveats below) can create two
children for one attempt — so the list makes no claim about which entry is
"current." Each entry links to `/jobs/{id}`, subject to the child's own
ACL.

### Caveats a cross-team caller must know

These apply to every `type: task` child today, same-workspace or not, and
are tracked as carried risks for a separate fix:

- **Double dispatch.** Two concurrent settlement passes on the same job can
  both see the same step ready and both create a child — the owner's task
  runs twice.
- **A stale dispatcher can fail a winner's step.** A dispatcher working
  from a stale read that then hits a render or default-merge error fails
  the step even though another dispatcher already succeeded and created a
  child.
- **Cancellation can miss a child** committed just after the cancellation's
  enumeration snapshot, by a dispatcher working from an earlier readiness
  read.
- **A timed-out parent step does not cancel its child.** The child's later
  completion is still written over the step; with a step retry, the stale
  child can settle the *replacement* attempt instead.
- **Removing a task that other workspaces call strands their running
  jobs.** A job whose task is removed mid-run is not settled by its own
  steps; absent a job timeout or an operator cancel, it stays `running`
  and the caller's step waits indefinitely.
- **A child whose first step is an approval fires no `on_suspended` hook**
  — it waits silently until someone notices and approves it directly.
- **Terminal delivery is not atomic.** A crash between a child's one-shot
  terminal claim and the write to the parent step loses that delivery.

### Errors calling a task

`task: nope.deploy` (unknown workspace), `task: platform.nope` (no such
task), and a task that references itself, are all `400` at submit; the
task owner being temporarily unavailable is `500`. This check runs for
**every** `type: task` action reference, not just a cross-workspace
(qualified) one — a plain, same-workspace `task: deploy` naming a task
that does not exist is also a `400` at submit, and it runs whether or not
the flow step has a `when` guard (only the *literal connection input*
check below is skipped for a `when`-guarded step). Only the submitted
task's own steps are checked at submit time — a nested reference that
fails resolves when its step is dispatched, and fails that step only.

## Errors

An unresolvable action reference — either the named workspace doesn't exist, or the workspace exists but has no action by that name (for example a typo like `jobs.recalc-agg-session`, missing the trailing `s`) — returns `400 Bad Request` with a precise message, never a `500`. The same fix applies to a missing/misnamed local connection reference.

This does **not** cover the owner workspace being transiently unavailable (for example, a Git-backed workspace that failed to load on its last poll) — that's a server/load-health condition, not a caller mistake, and still surfaces as `500`.

A step guarded by a `when` condition still has its `type: task` action's target task existence checked at job creation (see above) — a missing task is a config error regardless of `when`. What a `when` guard skips is the *literal connection input* pre-check, since the condition may make the step never run: a bad literal connection value on a `when`-guarded step surfaces as a step failure only if and when that step is actually reached.

## Not yet supported

The following are deliberately out of scope for this release:

- **Cross-workspace agent actions.** An `agent` step that is a cross-workspace reference still renders its prompt, system prompt, and MCP/task tools against the *caller's* workspace config, not the owner's — only script/docker/pod action bodies (and their connection-typed inputs) render in the owner context.
- **Cross-workspace hook actions.** `on_success`/`on_error`/`on_cancel`/`on_suspended` hook actions are not resolved cross-workspace — only flow-step `action:` references are. A `type: task` hook action naming another workspace's task is rejected at runtime, before any hook job is created: no hook job is created at all, and the error is logged to the *source* job (the job whose completion fired the hook), not to a hook job. (A config-load-time validator that would catch this when the workspace is loaded exists in `stroem-common` but is not yet wired into any server load/reload path — this is a pre-existing gap tracked in `docs/internal/TODO.md`, not something this release added.)
