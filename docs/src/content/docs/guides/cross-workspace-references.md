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

## Open access

Any workspace may reference any other workspace's actions — there is no ACL gate on cross-workspace references in this release. This is an intentional, low-friction choice for deployments where all workspaces are internal and already gated at the task-execution boundary.

One consequence worth knowing: a cross-workspace connection reference resolves the owner's secret values and passes the resolved object into the caller's job (subject to the same redaction rules as any connection input). Referencing another workspace's action or connection is effectively a way to read that workspace's effective connection values — not a leak, but a capability every workspace author has today. There's no per-workspace opt-out or `exports:` allowlist yet.

## Before / after example

This mirrors the incident that motivated this feature: `ai_traffic_model`'s `daily` task needed to call `jobs.recalc-agg-sessions` and use the `jobs` workspace's `clickhouse-prod` connection, without either being configured locally.

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

- Job creation succeeds — there is no local `clickhouse-prod` lookup, so no error.
- The `recalc` step is stamped with the owner workspace (`jobs`) and its pinned revision.
- The worker fetches the `jobs` tarball for that step, so files like `agg_sessions_4.sql` are present, and `clickhouse-prod` resolves using `jobs`' own secrets.

## Not yet supported

The following are deliberately out of scope for this release:

- **Qualified connection references outside a cross-workspace action.** An explicit `jobs.clickhouse-prod` reference from an arbitrary field (not the connection input of a `jobs`-owned action) is not resolved. Within a cross-workspace *action*, the action's own connection-typed inputs already resolve correctly by bare name in the owner context — that's what makes the example above work — but a bare `workspace.connection-name` reference used anywhere else is not yet supported.
- **Qualified connection-type references** (for example, a caller declaring an input `type: jobs.clickhouse`) are not yet supported.
- **Cross-workspace `type: task` actions.** A `task:` action referencing another workspace's task (`task: jobs.some-task`) is not yet resolved — only flow-step `action:` references are cross-workspace-aware today.
- **Cross-workspace agent actions.** An `agent` step that is a cross-workspace reference still renders its prompt, system prompt, and MCP/task tools against the *caller's* workspace config, not the owner's — only script/docker/pod action bodies (and their connection-typed inputs) render in the owner context.
- **Error responses.** An unresolvable dotted reference (no such library, no such workspace, or no such item in the named workspace) returns `400 Bad Request` with a precise message, never a `500` — this applies to both action lookups and connection-input resolution.
