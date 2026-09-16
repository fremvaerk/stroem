# Cross-Workspace `type: task` Actions — Design

Status: revision 1, proposed
Ships in: 0.17.0 (minor; no migration; one new resolution rule, one latent bug fix)

Closes the first item under "Deferred" in CLAUDE.md § Cross-Workspace
References and "Not yet supported" in
`docs/src/content/docs/guides/cross-workspace-references.md:245`. Line numbers
cite `main` at `f174020`.

## Revision history

**Revision 1 (2026-09-16).** Initial design. Decisions taken in the
brainstorm, in order: the driving use case is cross-team pipelines (workspace
A's flow kicks off workspace B's real task and waits for its result, B owning
its secrets and connections); access is open, like cross-workspace actions —
no `shared:` opt-in on tasks; both reference forms are supported (§3.1); no
child-level ACL check (§3.6); resolve at dispatch with no schema change
rather than pinning the owner at creation (§2, approach 1).

## 1. Problem

A flow step can already call another workspace's *action*: `action:
jobs.recalc` resolves against the `jobs` workspace at job creation
(`job_creator.rs:324-372`), the step row is stamped `action_workspace` /
`action_revision` (`:397-408`, migration 043) and the worker fetches the
owner's tarball. A flow step cannot call another workspace's *task*:

- A local `type: task` action with `task: jobs.deploy` is looked up flat in
  the caller's `tasks` map at dispatch (`dispatch.rs:131-133` →
  `job_creator.rs:213`) and fails with `Task 'jobs.deploy' not found in
  workspace 'caller'`.
- `action: jobs.run-deploy`, where `jobs` defines `run-deploy` as `type:
  task, task: deploy`, resolves at creation like any cross-workspace action
  and is stamped `action_workspace = "jobs"` — but dispatch never reads that
  column (grep: `action_workspace` does not occur in
  `settlement/dispatch.rs`), calls `create_job_for_task_inner` with the
  **caller's** config (`dispatch.rs:259-274`), and fails the same way. It
  also merges the action's input defaults from the **caller's** actions map
  by bare name (`dispatch.rs:206`), so a caller action that happens to share
  the name would be used instead. This is latent today only because the
  step always fails before it matters.

Cross-team pipelines need the child to be a real workspace-B job: B's
config, B's current revision, B's secrets and connections, B's task-level
hooks, visible in B's job list, ACL'd under B's rules — while the parent
step in A receives its output exactly as a same-workspace child's.

## 2. Approach

**Resolve at dispatch; no schema change.** The owner of the task is derived
when the `type: task` step is dispatched, from columns the step row already
carries, and the child job is created in the owner workspace with the
owner's currently loaded config and revision.

Rejected: pinning `task_workspace` / `task_revision` on `job_step` at parent
creation (a migration, B's revision pinned by A's clock, and the config used
to build the child is the current one regardless, so the pin would be partly
cosmetic); and normalising form A into form B by stamping
`action_workspace` on the step (`action_workspace` means "who owns the
*action*" — it drives the action-defaults merge and the worker's tarball
choice — and form A's action is local).

## 3. Design

### 3.1 The resolution rule

Two reference forms converge on one rule.

| Form | YAML in caller `ws_a` | Step row at creation |
|---|---|---|
| A — direct | local action `type: task`, `task: ws_b.deploy` | `action_workspace = NULL`, `action_spec.task = "ws_b.deploy"` |
| B — via owner action | `action: ws_b.run-deploy`; in `ws_b`: `run-deploy: {type: task, task: deploy}` | `action_workspace = "ws_b"`, `action_spec.task = "deploy"` |

**base** = `step.action_workspace` ?? `job.workspace`. `task_ref` is resolved
relative to base:

1. `base_cfg.tasks.get(task_ref)` — a hit is local to base. This is where
   library-flattened names (`common.deploy`) land, keeping the "library
   first, workspace on miss" precedence that action references use
   (`job_creator.rs:328-334`).
2. Miss, and `task_ref` is dotted: `parse_qualified_ref`
   (`template.rs:91`) → `(ws, name)`. `ws` must satisfy
   `workspaces.has_workspace(ws)` (`workspace/mod.rs:528`, same detection
   as action refs); `workspaces.get_config(ws)` (`:452`) `None` means
   configured-but-unavailable; `name` must be a key of that config's
   `tasks`.
3. Miss, not dotted: today's `Task '…' not found in workspace '…'`.

Form B with a foreign `task:` inside the owner (`ws_b`'s `run-deploy` says
`task: ws_c.x`) resolves relative to `ws_b` by the same rule — nothing
special-cases it.

One function owns the rule:

```rust
// crates/stroem-server/src/job_creator.rs
pub(crate) struct ResolvedTask {
    pub workspace: String,           // owner
    pub config: Arc<WorkspaceConfig>, // owner's currently loaded config
    pub task_name: String,           // bare
}
pub(crate) async fn resolve_task_ref(
    workspaces: &WorkspaceManager,
    base_ws: &str,
    base_cfg: &Arc<WorkspaceConfig>,
    task_ref: &str,
) -> Result<ResolvedTask>;
```

Errors, all `anyhow!` with no further `.context()` so they are the
**outermost** message (`classify_execute_error` matches the legacy phrases
on the outermost message only, `web/api/mod.rs:404-411`):

| Condition | Message | HTTP at creation |
|---|---|---|
| dotted, `ws` not configured (or a library prefix that does not exist) | `task '{ref}': unknown workspace '{ws}'` | 400 (`"unknown workspace"` is already a precise phrase, `:398`) |
| `ws` configured but `get_config` is `None` | `task '{ref}': workspace '{ws}' is not available` | 500 (`:394`) |
| `ws` loaded, no such task | `task '{ref}': workspace '{ws}' has no task '{name}'` | 400 — **new** phrase `"has no task"` added to the legacy list beside `"has no action"` (`:407`) |
| not dotted, missing | unchanged (`job_creator.rs:213-218`) | 400 (`"not found"`) |

`has_workspace` ignores `load_errors` (the TODO at `workspace/mod.rs:519-527`);
a workspace whose *source construction* failed therefore reads as unknown
(400) here, exactly as it does for action references today. Not changed by
this design.

### 3.2 Creation-time pre-check

In the step loop of `create_job_for_task_inner`, after the action is
resolved (`job_creator.rs:373`) and before `build_step`: if
`action.action_type == "task"`, call `resolve_task_ref` with base =
`(action_workspace, owner_cfg)` when the action is cross-workspace, else
`(workspace_name, workspace_config)`, and discard the result. It is an
existence check so a bad reference is a 400 at submit time rather than a
failed step later. It runs regardless of `when` — a missing task is a
config error, unlike a possibly-unused connection
(`precheck_literal_connection_inputs` skips `when`-guarded steps for that
reason; this check does not). It does **not** run in `build_step` itself:
hook jobs also go through `build_step`, and hook actions are not resolved
cross-workspace (still deferred), so a `type: task` hook action keeps its
current flat lookup.

Nothing new is stamped on the step row. `action_workspace` /
`action_revision` keep their meaning (the action's owner) and are set
exactly as today.

### 3.3 Dispatch

`settlement/dispatch.rs::handle_task_steps_pass`, per `type: task` step:

1. **Base.** `base_ws = step.action_workspace.as_deref().unwrap_or(&job.workspace)`.
   `base_cfg` is `workspace_config` when `base_ws == workspace_name`, else
   `workspaces.get_config(base_ws)`; `None` → `fail_task_step` with
   `workspace '{base_ws}' is not available` (owner reloaded into a
   placeholder between creation and dispatch).
2. **Resolve.** `resolve_task_ref(workspaces, base_ws, &base_cfg,
   action_spec.task)`; `Err` → `fail_task_step` with the message (owner
   dropped the task since creation). Both failure branches go through
   `fail_task_step` (`dispatch.rs:63`) so dependents cascade-skip and the
   job closes — the rule from § Task Actions.
3. **Step input** still renders in the **caller's** context
   (`Scope::ChildTaskInput`, `dispatch.rs:158-163`). Unchanged.
4. **Action-level defaults** (`dispatch.rs:206-236`): look up
   `step.action_name` in `base_cfg.actions`, not `workspace_config.actions`,
   and merge with `prepare_action_input_cross(&rendered, &action.input,
   &ws_set, workspace_name, base_ws)` (`template.rs:851`) instead of
   `prepare_action_input`. When base is the caller the two calls are
   identical (`:850`), so same-workspace behaviour is byte-for-byte
   unchanged. This is the fix for the latent bug in § 1.
5. **Caller-supplied connection values for the child's task input.** When
   `resolved.workspace != workspace_name`, run
   `resolve_connection_inputs_scoped(&rendered, &task.input, &ResolveScope
   { lookup: &ws_set, schema_ws: owner, value_ws: caller, fallback_ws:
   Some(owner) })` (`template.rs:580`, `:226-236`) on the rendered input
   before creating the child. This is the same provenance rule
   cross-workspace actions apply to caller-supplied values
   (`cross-workspace-references.md:125-133`): a bare name resolves in the
   caller first, then in the owner **only if `shared: true`**; a qualified
   `ws.conn` goes through the ordinary gate. Fields the caller did not
   supply are left for the creator, which fills them from the owner's
   defaults and resolves them ungated in the owner (step 6). Resolved
   objects pass through the creator's own `resolve_connection_inputs`
   untouched (`template.rs:600-602`). An error here → `fail_task_step`.
   Same-workspace children skip this pass.
6. **Create** with the **owner's** config and name:
   `create_job_for_task_inner(workspaces, pool, &resolved.config,
   &resolved.workspace, &resolved.task_name, rendered_input, "task",
   Some(&source_id), Some(job_id), Some(&step.step_name), revision,
   CreationMode::Normal, None, defaults)`. Inside, `merge_defaults` uses
   the owner's secrets (`job_creator.rs:281-283`) and connection resolution
   uses `WorkspaceSet::load(workspaces, owner, …)` (`:315-317`) — exactly
   what an HTTP execute of `ws_b/deploy` does. `raw_input` is the rendered
   caller input (`:278`).
7. **Revision.** `revision = if resolved.workspace == job.workspace {
   job.revision } else { workspaces.get_revision(&resolved.workspace) }`
   (`workspace/mod.rs:473`). Same-workspace children keep inheriting the
   parent's revision; a foreign child gets the owner's revision at the
   moment it is created — the value the owner's own triggers would use.
   This deliberately differs from cross-workspace *actions*, whose owner
   revision is pinned at parent creation (`cross-workspace-references.md:48`);
   the difference is documented (§ 6).
8. **Log.** The `tracing::info!` at `dispatch.rs:279` and the parent job's
   creation line gain ` in workspace '{owner}'` when the owner differs.
   `MAX_TASK_DEPTH` (`dispatch.rs:136-137`) is checked before any of this,
   as today.

The child row is stamped `workspace = owner`, `task_name = bare`,
`parent_job_id`, `parent_step_name`, `source_type = "task"` — the last three
as today (`dispatch.rs:266-269`).

### 3.4 What does not change — verified

Every consumer downstream of creation keys off the **child row's own**
`workspace` / `task_name`, so a child stamped with the owner workspace is
handled correctly with no code change:

- **Propagation** — `Settlement::propagate` writes the parent step from
  `child_job.status` / `.output` and calls `advance(parent_job_id)`
  (`propagate.rs:25-151`); `advance` re-reads the parent row and resolves
  *its* workspace (`settlement/mod.rs:201`, `:135-136`). The parent's
  cascade runs under the parent's config.
- **Terminal handling, hooks** — resolved from the terminal job's own row
  (`settlement/mod.rs:135-169`). A child is not a top-level source
  (`hooks.rs:78-83`), so only the owner *task's* `on_*` hooks fire, in the
  owner workspace (`hooks.rs:284-293`); the caller's workspace-level hooks
  never see the child. Same as a same-workspace child.
- **ACL** — no per-child check exists today: `execute_task` checks `Run`
  on the top-level task only (`web/api/tasks.rs:435-477`) and server
  dispatch never consults ACL. Unchanged (decision, § revision 1). Job
  detail / logs / artifacts check `View` against the row's
  `(workspace, task_name)` (`web/api/jobs.rs:1141-1161`), so the child is
  governed by the **owner's** rules: a caller with `Run` on `ws_a/pipeline`
  and nothing on `ws_b` sees the step's output on the parent but gets 403
  on the child job.
- **Log archive key** (`terminal.rs:132-138`), **state snapshots**
  (`settlement/mod.rs:215-221` — the child reads `ws_b/deploy`'s state;
  the *step input* in the caller still renders with the caller's
  snapshots, step 3 above), **artifacts**, **restart / re-run rejection**
  (`is_top_level_job`: `source_type = "task"` + `parent_job_id`, already
  rejected), **`compute_depth`** (`job_creator.rs:600`, walks
  `parent_job_id` only), **UI Job Detail** (`job-detail.tsx:65-96` keys off
  the fetched row).
- **Redaction** of the child's detail already covers every loaded
  workspace's secrets (`workspace_set::collect_redaction_values`).

### 3.5 Cycles

`check_task_self_reference` (`validation.rs:1048-1069`) compares flat names
within one config and stays flat: a qualified reference can only be a
self-reference if it names the same workspace and task, which rule 1
already reduces to the bare name. A cross-workspace cycle (`ws_a/p` →
`ws_b/q` → `ws_a/p`) cannot be seen by any one workspace's validation and
is bounded by `MAX_TASK_DEPTH` (10) like any other nesting — the step at
depth 10 fails with the existing message and the chain closes. Documented;
detection is a follow-up (TODO.md).

### 3.6 Security posture

Unchanged from cross-workspace actions: open reference, no opt-in, the
top-level `Run` check is the authorization. What a foreign caller can do
with `ws_b/deploy` is exactly what `ws_b`'s own scheduler trigger can do
with it — run it with its own defaults and any input the schema accepts.
The caller cannot reach `ws_b`'s *unshared* connections by name (step 5)
and cannot read `ws_b`'s secrets: only the child's output crosses back,
and the child's detail is `ws_b`-ACL'd and redacted.

## 4. Validation (`stroem-common`)

`validate_workflow_config_inner`'s `type: task` check (`validation.rs:65-78`):

- CLI path (`libraries_resolved == false`): a dotted `task:` that is not a
  local key is skipped **with a warning** (`cannot validate cross-workspace
  task reference '…' offline`) — today it is skipped silently as a
  presumed library name (`:69-70`); the warning matches what action refs
  get.
- Server path: `CrossWorkspaceActionResolver` (`validation.rs:19`, `&dyn
  Fn(&str, &str) -> bool`) is generalised to a small trait with
  `has_action(ws, name)` and `has_task(ws, name)`, both `-> bool`; the
  existing closure call sites become `has_action`. A dotted `task:` that
  misses locally is accepted iff `has_task(ws, name)`; with no resolver
  (`validate_workflow_config_with_libraries`) it is an error, as today.
  `validate_workflow_config_with_cross_workspace_resolver` is still not
  wired into any server load path (pre-existing gap, CLAUDE.md); the
  creation-time pre-check (§ 3.2) is the enforcement.

## 5. Tests

Each pins one seam. Integration tests use the existing multi-workspace
fixture (`setup_multi_workspace_with`, `integration_test.rs`).

- **`stroem-common` unit**: dotted `task:` — CLI skip with warning; server
  accept via `has_task`; server reject when `has_task` is false; a local
  library-flattened key wins over the split.
- **`job_creator` unit** (`resolve_task_ref`, in-module): local hit,
  library-flattened hit, qualified hit, unknown workspace, unavailable
  workspace, loaded-but-no-task; each error's exact phrase.
- **Integration**:
  - Form A creates a child with `workspace = ws_b`, `task_name = "deploy"`,
    `revision = ws_b`'s revision (≠ parent's), `parent_job_id` set;
    completing the child propagates its output to the parent step and the
    parent job completes.
  - Form B does the same; the owner action's input default is merged from
    `ws_b`'s action while a same-named caller action with a different
    default is ignored (the `dispatch.rs:206` regression).
  - Form B where the owner's `task:` is itself qualified to a third
    workspace resolves relative to the owner.
  - Execute returns 400 for `task: nope.deploy` (unknown workspace) and
    `task: ws_b.nope` (has no task), 500 when `ws_b` is a placeholder;
    `classify_execute_error` unit test for `"has no task"`.
  - Owner task removed between creation and dispatch (reload `ws_b`
    without it): step fails, dependents skip, job fails, nothing stuck
    `running`.
  - Caller-supplied bare connection name on a foreign child: resolves in
    the caller if present; falls back to the owner only when `shared:
    true`; unshared → step fails with `is not shared`. Owner-default
    connection on the task input resolves ungated.
  - ACL: user with `Run` on `ws_a/pipeline` and no rule on `ws_b` — 200 on
    the parent, 403 on the child's detail. Admin sees both.
  - Owner task's `on_error` hook fires in `ws_b`; the caller workspace's
    `on_error` does not fire for the child.
  - Same-workspace child still inherits the parent's revision and behaves
    as before (regression).
- **`tests/e2e.sh`**: one cross-workspace `type: task` step, server ↔
  worker, output round-trips to the parent.

## 6. Documentation

- `docs/src/content/docs/guides/cross-workspace-references.md`: new section
  "Calling tasks in other workspaces" — both forms, the base/precedence
  rule, "input renders in the caller, the child runs as the owner", the
  revision rule and how it differs from actions (§ 3.3 step 7), ACL and
  visibility, hooks, the depth bound for cycles; remove the first "Not yet
  supported" bullet (`:245`).
- `CLAUDE.md` § Cross-Workspace References: replace the deferred bullet with
  the resolution rule and name `resolve_task_ref` as its owner; note the
  revision difference. `CONTEXT.md`: "Owner workspace" entry if absent.
- `docs/internal/TODO.md`: add "cross-workspace task cycle detection" and
  "cross-workspace hook actions" (still deferred) if not already listed.

## 7. Out of scope

Cross-workspace hook actions and cross-workspace `agent` steps rendering
against the owner (both still deferred, unchanged); a `shared:` flag on
tasks (decided against); per-child ACL (decided against); pinning the owner
revision at parent creation (rejected, § 2); event-source backpressure
counting across workspaces (unrelated to `type: task` dispatch).
