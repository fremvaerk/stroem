# Cross-Workspace References — Design

**Status:** Draft (awaiting review)
**Date:** 2026-07-29

## 1. Problem

Today, workspaces in Strøm are fully isolated. A flow step's `action:` is looked up
only in the step's own workspace (`job_creator.rs`: `workspace_config.actions.get(&flow_step.action)`),
and connection-typed inputs are resolved only against the local workspace
(`resolve_connection_inputs(..., workspace_config)`). The **only** way to share an
action/task/connection-type across workspaces is to register a source as a
**library** (server-config `libraries:`), which merges items into every workspace
under a `libname.` prefix.

This surfaced as a production incident: the `ai_traffic_model` `daily` task was
edited (commit `751bd21`) to call `jobs.recalc-agg-sessions` — an action that lives
in the `jobs` workspace — and to take a `clickhouse` input defaulting to the
`clickhouse-prod` connection (also defined only in `jobs`). Because there is no
`jobs` **library** configured, and connections are workspace-scoped:

- Resolving the `daily` task's `clickhouse` input against `ai_traffic_model` fails
  with *"connection 'clickhouse-prod' … does not exist"*, returned as **HTTP 500**.
- Even with that fixed, `jobs.recalc-agg-sessions` would not resolve as an action.

The operator's intent — "reference another workspace's action/connection directly,
without setting up a library" — is not currently supported.

## 2. Goals / Non-Goals

**Goals**
- Reference another workspace's **action**, **task**, **connection**, and
  **connection-type** by a qualified `workspace.item` name, with no library setup.
- A cross-workspace action executes with **its owner workspace's** files, secrets,
  and connections; the caller passes only plain inputs.
- Fully backward-compatible: unqualified names keep resolving locally; existing
  library references keep working.

**Non-Goals**
- No access control between workspaces in this iteration (explicitly chosen: **open**
  — any workspace may reference any other; see §8). An opt-in/ACL model can be layered
  on later without changing the reference syntax.
- No change to the library mechanism itself.
- No cross-workspace `{{ secret.other_ws.* }}` template access (secrets stay local to
  the workspace whose templates are being rendered; a qualified *connection* is the
  supported way to reach another workspace's credentials).

## 3. Reference Model

A dotted name `foo.bar` is resolved in this precedence order:

1. **Library** `foo` (existing behavior — libraries are merged into the workspace's
   config at load time under the `foo.` prefix, so `foo.bar` is already a local key).
2. **Workspace** `foo`, item `bar` (new — resolved live against the other workspace's
   loaded config).

Because libraries are already flattened into the local config before job creation,
step 1 is automatic (`workspace_config.actions.get("foo.bar")` hits). Step 2 is the
new fallback: on a local miss, split on the **first** `.` into `(ws, item)` and look
`item` up in workspace `ws`'s config via the `WorkspaceManager`.

This applies uniformly to:
- Flow step `action:` → cross-workspace **action**.
- `type: task` action `task:` → cross-workspace **task** (creates a child job in the
  owner workspace; see §6).
- Connection references (a connection-typed input's value/default) → cross-workspace
  **connection**, e.g. `clickhouse-prod` referenced from a step whose action is owned
  by `jobs` resolves in `jobs`; or an explicit `jobs.clickhouse-prod` from anywhere.
- Connection-type of an input field (`type: jobs.clickhouse`) → cross-workspace
  **connection-type** (needed only when a *caller* wants to declare a typed input for
  a connection that lives in another workspace).

Unqualified names are unchanged (local-only). Names with no matching library **and**
no matching workspace produce a clear `BadRequest` (see §9), never a 500.

## 4. Resolution Semantics (owner context)

The key rule: **a cross-workspace item resolves in the workspace that owns it.**

- **Connection** `jobs.clickhouse-prod`: the connection object's templates
  (`{{ secret.clickhouse.* }}`, etc.) render against the **jobs** workspace's secrets
  and config. The resolved values object is what flows onward. The caller needs no
  local connection, connection-type, or secret.
- **Action** `jobs.recalc-agg-sessions`: the action definition comes from `jobs`. When
  it runs, its own `env:`/`args:`/`script`/`source:` templates and any unqualified
  `{{ secret.* }}` / connection references resolve against **jobs** (its files and
  secrets). The caller supplies only the flow-step `input:` values.

### Two render contexts, split at the step boundary

For a cross-workspace step the two halves render in **different** contexts:

1. **Flow-step `input:` map** (caller side): templates like `{{ run.output.dataset }}`
   or `{{ input.clickhouse }}` reference the *caller's* job (previous step outputs, the
   caller task's inputs, caller/qualified connections). Rendered in the **caller's**
   workspace/job context, exactly where the local case renders it today.
2. **Action body** (owner side): the owner action's `env:`/`args:`/`script` render
   against the **owner** workspace, using (a) the concrete input values passed from
   step 1 and (b) the owner's secrets/connections/files.

Connection-typed inputs of a cross-workspace action are resolved against the
**owner** workspace. This is what makes the `daily` fix clean: the caller passes
`clickhouse: "clickhouse-prod"` (a plain name) and `jobs` resolves it — no
ClickHouse config needed in `ai_traffic_model` at all.

## 5. Execution Model — how the worker gets the files

**No new worker machinery.** Strøm already dispatches work **per step**, and each
claimed step carries its own `workspace` + `revision`; the worker fetches exactly that
one workspace via the immutable, ref-counted `WorkspaceCache`:

```rust
// crates/stroem-worker/src/poller.rs
let ws_result = if let Some(ref rev) = step.revision {
    ws_cache.ensure_revision(&client, &step.workspace, rev).await
} else {
    ws_cache.ensure_up_to_date(&client, &step.workspace).await
};
```

Today every step's `workspace` equals the job's workspace. The change: for a
cross-workspace action step, the **server stamps the step's `workspace` = owner
workspace and `revision` = the owner's pinned revision**. The worker then downloads
the owner tarball (cache-keyed by `(workspace, revision)`) and runs the step there —
so, e.g., `recalc_ch_agg/agg_sessions_4.sql` is present.

Consequences:
- A step downloads **one** workspace (its owner), never two. Across a job a worker may
  cache both the caller's and the owner's tarballs because different steps need
  different ones — reusing existing cache logic, no per-step double-download.
- **Revision pinning:** the owner workspace's revision is pinned at job-creation time
  (same discipline as `job.revision`), so a mid-run change to `jobs` doesn't shift
  under an in-flight `ai_traffic_model` job.

## 6. Server-Side Flow

At job creation (and at for-each/promotion claim time, where local resolution already
happens):

1. **Action lookup.** For each flow step, resolve `flow_step.action`:
   local hit → as today. Local miss with a `.` → split `(owner_ws, action_name)`,
   fetch `owner_ws`'s config from the `WorkspaceManager`, look up `action_name`.
   Miss on both → `BadRequest`.
2. **Stamp owner on the step.** Persist `action_workspace` (owner) and
   `action_revision` (owner's current revision) on the `job_step`. Default both to the
   job's own workspace/revision for local steps (or leave NULL and interpret NULL as
   "job's workspace" — see §7).
3. **`action_spec`, `required_ability`, `required_tags`, `runner`, retry** derive from
   the **owner** action definition.
4. **Input resolution.** The flow-step `input:` map renders in the caller context
   (unchanged). Connection-typed inputs of a cross-workspace action are resolved
   against the **owner** workspace config (new: resolution must be workspace-aware —
   see §10).
5. **`type: task` cross-workspace actions.** A `task: owner_ws.some_task` creates a
   child job **in `owner_ws`** (child `job.workspace = owner_ws`), reusing the existing
   child-job path; depth/self-reference guards unchanged.

## 7. Data Model

Add to `job_step` (new migration, additive, nullable):

- `action_workspace TEXT NULL` — owner workspace of the step's action. `NULL` ⇒ the
  job's own workspace (keeps existing rows valid; no backfill needed).
- `action_revision TEXT NULL` — pinned owner revision. `NULL` ⇒ use the job's
  `revision` (i.e. `ensure_up_to_date` / current behavior).

The worker's claimed-step payload gains these two fields; when present they drive
`step.workspace` / `step.revision` for the fetch in §5. When absent, behavior is
byte-for-byte the current behavior.

## 8. Access Control

**Open** (chosen): any workspace may reference any other workspace's items. Rationale:
all workspaces in this deployment are internal/trusted and already admin-gated at the
task-execution boundary; lowest friction; matches the "reference any workspace" intent.

Exposure to document clearly: a cross-workspace **connection** reference resolves the
owner's secret values and passes them into the caller's job (as a resolved values
object, subject to the same redaction rules as any connection input). This is an
intentional capability, not a leak — but it means any workspace author can read any
connection's effective values by referencing it. If future isolation is wanted, the
reference **syntax stays the same**; we'd add an owner-side `exports:` allowlist or an
ACL check at resolution time (both were considered and deferred).

## 9. Error Handling

- Unresolvable dotted reference (no library, no such workspace, or no such item in the
  named workspace) → **`AppError::BadRequest`** with a precise message
  (`"action 'jobs.recalc-agg-sessions': workspace 'jobs' has no action 'recalc-agg-sessions'"`),
  **not** 500.
- **Related fix (independent, ship first):** connection-resolution failures currently
  surface as **500 Internal** (`resolve_connection_inputs` → `anyhow` →
  `From<anyhow::Error>` → `Internal`). A missing/misnamed connection is a user error →
  should be `BadRequest`. This is the root cause of the opaque "internal server error"
  in the incident and is worth fixing on its own, ahead of this feature.

## 10. Surface Area / Touch Points

- **stroem-common**
  - `template.rs::resolve_connection_inputs` — make workspace-aware: accept a resolver
    (e.g. `&dyn Fn(&str) -> Option<&WorkspaceConfig>` or the manager handle) so a
    qualified/owner connection name resolves in the correct workspace. Today it takes a
    single `&WorkspaceConfig`.
  - A small `parse_qualified_ref(name) -> (Option<workspace>, item)` helper
    (`split_once('.')`), plus validation updates so dotted names are accepted when the
    target workspace/item exists.
- **stroem-server**
  - `job_creator.rs` — cross-workspace action lookup; stamp `action_workspace` /
    `action_revision`; owner-context connection/input resolution; `type: task`
    child-job-in-owner path.
  - Claim/promotion path — same resolution for for-each/`when`/state steps.
  - `web/error.rs` — connection-resolution error → `BadRequest` (see §9).
  - DB repo + **migration** for the two new `job_step` columns; include columns in the
    claimed-step query/serialization.
- **stroem-worker**
  - `poller.rs` — populate `step.workspace` / `step.revision` from the new fields
    (default to job's when NULL). Likely a 1–3 line change; `WorkspaceCache` unchanged.
- **Validation** (`validation.rs`) — a dotted `action`/`task`/connection/type reference
  is valid if it resolves as a library item **or** as `workspace.item` for a known
  workspace. CLI `stroem validate` (no server context) keeps skipping dotted names with
  a warning; the **server** validates fully after workspaces load.
- **Docs** — new "Cross-workspace references" guide; contrast with libraries; document
  the open-access exposure and revision pinning.

## 11. Testing Plan

- **Unit** (stroem-common): `parse_qualified_ref`; `resolve_connection_inputs` with a
  qualified/owner connection resolving in another workspace's context (secrets from the
  owner); precedence (library beats same-named workspace).
- **Unit/integration** (stroem-server): job creation with a cross-workspace action
  stamps `action_workspace`/`action_revision` and the owner `action_spec`;
  connection-typed input of a cross-workspace action resolves in the owner; unknown
  workspace/item → `BadRequest` (assert status + message); `type: task` cross-workspace
  creates the child job in the owner workspace; revision pinning (owner reload mid-job
  doesn't change the pinned revision).
- **Migration test** — existing `job_step` rows (NULL columns) execute exactly as
  before.
- **E2E** (`tests/e2e.sh`) — two folder workspaces `A` and `B`; `A`'s task calls
  `B.some-action` that reads a file only present in `B` and uses `B`'s connection;
  assert the step runs with `B`'s files and completes.
- **Regression** — the exact incident: an `ai_traffic_model`-shaped task calling a
  `jobs`-shaped action + connection succeeds without any ClickHouse config in the
  caller.

## 12. Rollout

- Additive migration; NULL-safe defaults ⇒ zero behavior change for existing jobs.
- Ship the **500→400 connection-error fix (§9)** first (tiny, independently valuable).
- Build the feature on a dedicated worktree/feature branch; spec → plan → implement.
- After merge + release, migrate the `ai_traffic_model` `daily`/`backfill` tasks to the
  clean form (drop the local `clickhouse` input; pass `clickhouse: "clickhouse-prod"`
  to the `jobs.recalc-agg-sessions` step, resolved in the owner context).

## 13. Worked Example (target state)

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

- Job creation succeeds (no local `clickhouse-prod` lookup → no 500).
- The `recalc` step is stamped `action_workspace = jobs`, `action_revision = <pinned>`.
- The worker fetches the `jobs` tarball for that step; `agg_sessions_4.sql` is present;
  `clickhouse-prod` resolves with `jobs`' secrets.
