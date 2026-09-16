# Cross-Workspace `type: task` Actions — Design

Status: revision 2, proposed
Ships in: 0.17.0 (minor; no migration; documented behaviour corrections, § 8)

Closes the first item under "Deferred" in CLAUDE.md § Cross-Workspace
References and "Not yet supported" in
`docs/src/content/docs/guides/cross-workspace-references.md:245`. Line numbers
cite `main` at `f174020`.

## Revision history

**Revision 2 (2026-09-16).** Revision 1's review (Codex, session
`01a0a91a`) found ten defects, all verified against the code; none were
wrong. Four were design defects: the connection pass in dispatch could not
deliver the isolation § 3.6 promised (a primitive task input templated into
a connection name binds in the owner's own scope, `template.rs:258-265`,
`rendering.rs:125-129`); the input pipeline lost provenance between the
action's defaults and the task's schema, and an object resolved against the
action schema bypassed the task schema's type check (`template.rs:600-602`);
§ 1's "always fails" premise was false — a caller task with the same bare
name is invoked *today* (`job_creator.rs:213`); and `task: A.p` inside `A`
slipped past self-reference validation (`validation.rs:1054`) while a
qualified task in a hook validated but failed at runtime (`hooks.rs:566`).
Three were overclaims: "cannot read B's secrets" (output propagates
verbatim, and `fail_task_step` scrubbed only the caller's secrets,
`dispatch.rs:77-78`); "owner's current config and revision" is not one
snapshot (`workspace/mod.rs:452`, `:473`, `:664-680`); and a child is not
the owner's scheduler run (no task-level retry, `terminal.rs:180`; no
workspace-level hooks; cancel cascades without the owner's ACL,
`settlement/mod.rs:748`). Three were inherited lifecycle gaps that
same-workspace children already have: dispatch is neither exclusive nor
crash-safe and late child completion overwrites a timed-out parent step
(`job_step.rs:630-645`, `:665-686`, `recovery.rs:117-159`); a job whose
task vanishes mid-run is never settled (`settlement/mod.rs:209-211`); and a
child suspended at creation never fires `on_suspended` (`dispatch.rs:277-285`,
`:315`). Decisions: keep open access, stated as trusted delegation (§ 3.6);
fix all three lifecycle gaps in this change (§ 4) rather than carry them;
action-level defaults come from the persisted `action_spec`, not a live
lookup (§ 3.3 step 4); job detail returns 404, not 403, on Deny. A small
parent→child link on the step DTO is added (§ 3.7) because a cross-team
child is otherwise unreachable from the caller's UI.

**Revision 1 (2026-09-16).** Initial design. Decisions taken in the
brainstorm: the driving use case is cross-team pipelines (workspace A's flow
kicks off workspace B's real task and waits for its result, B owning its
secrets and connections); access is open, like cross-workspace actions —
no `shared:` opt-in on tasks; both reference forms are supported (§ 3.1); no
child-level ACL check (§ 3.6); resolve at dispatch with no schema change
rather than pinning the owner at creation (§ 2).

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
  column (`action_workspace` does not occur in `settlement/dispatch.rs`)
  and calls `create_job_for_task_inner` with the **caller's** config and the
  bare name `deploy` (`dispatch.rs:259-264`). If the caller has no task
  `deploy` the step fails; **if it has one, the caller's `deploy` runs** —
  the wrong task, silently. The action's input defaults are likewise merged
  from the caller's actions map by bare name (`dispatch.rs:206`).

Cross-team pipelines need the child to be a real workspace-B job: B's
config, B's revision, B's secrets and connections, B's task-level hooks,
visible in B's job list, ACL'd under B's rules — while the parent step in A
receives its output exactly as a same-workspace child's.

## 2. Approach

**Resolve at dispatch; no schema change.** The owner of the task is derived
when the `type: task` step is dispatched, from columns the step row already
carries, and the child job is created in the owner workspace from one
snapshot of the owner's currently loaded config.

Rejected: pinning `task_workspace` / `task_revision` on `job_step` at parent
creation (a migration, B's revision pinned by A's clock, and the config used
to build the child is the current one regardless); and normalising form A
into form B by stamping `action_workspace` on the step (`action_workspace`
means "who owns the *action*" and drives the worker's tarball choice; form
A's action is local).

## 3. Design

### 3.1 The resolution rule

Two reference forms converge on one rule.

| Form | YAML in caller `A` | Step row at creation |
|---|---|---|
| A — direct | local action `type: task`, `task: B.deploy` | `action_workspace = NULL`, `action_spec.task = "B.deploy"` |
| B — via owner action | `action: B.run-deploy`; in `B`: `run-deploy: {type: task, task: deploy}` | `action_workspace = "B"`, `action_spec.task = "deploy"` |

Three workspaces can be involved: the **caller** `A` (the job's workspace,
where the flow step and its `input:` live), the **action owner** `O` =
`step.action_workspace` ?? `A`, and the **task owner** `T`, resolved from
`action_spec.task` relative to `O`:

1. `O_cfg.tasks.get(task_ref)` — a hit means `T = O`. This is where
   library-flattened names (`common.deploy`) land, keeping the "library
   first, workspace on miss" precedence that action references use
   (`job_creator.rs:328-334`).
2. Miss, and `task_ref` is dotted: `parse_qualified_ref` (`template.rs:91`)
   → `(ws, name)`. `ws` must satisfy `workspaces.has_workspace(ws)`
   (`workspace/mod.rs:528`, the same detection action refs use);
   `workspaces.get_config(ws)` (`:452`) `None` means
   configured-but-unavailable; `name` must be a key of that config's
   `tasks`. `T = ws`.
3. Miss, not dotted: today's `Task '…' not found in workspace '…'`.

Form B with a foreign `task:` inside the owner (`B`'s `run-deploy` says
`task: C.x`) resolves relative to `B` by the same rule, giving `A ≠ O ≠ T`.

One function owns the rule:

```rust
// crates/stroem-server/src/job_creator.rs
pub(crate) struct ResolvedTask {
    pub workspace: String,            // T
    pub config: Arc<WorkspaceConfig>, // one snapshot of T's loaded config
    pub task_name: String,            // bare
}
pub(crate) async fn resolve_task_ref(
    workspaces: &WorkspaceManager,
    base_ws: &str,                    // O
    base_cfg: &Arc<WorkspaceConfig>,
    task_ref: &str,
) -> Result<ResolvedTask>;
```

Errors are `anyhow!` with no further `.context()` so they stay the
**outermost** message (`classify_execute_error` matches the legacy phrases
on the outermost message only, `web/api/mod.rs:404-411`):

| Condition | Message | HTTP at creation |
|---|---|---|
| dotted, `ws` not configured (or a library prefix that does not exist) | `task '{ref}': unknown workspace '{ws}'` | 400 (`"unknown workspace"`, `:398`) |
| `ws` configured, `get_config` is `None` | `task '{ref}': workspace '{ws}' is not available` | 500 (`:394`) |
| `ws` loaded, no such task | `task '{ref}': workspace '{ws}' has no task '{name}'` | 400 — **new** phrase `"has no task"` beside `"has no action"` (`:407`) |
| resolves to the task being created (`T == A && name == task_name`) | `task '{ref}' is a self-reference to '{A}/{task}' (invalid)` | 400 (`"invalid"`, `:409`) |
| not dotted, missing | unchanged (`job_creator.rs:213-218`) | 400 (`"not found"`) |

The HTTP outcome applies to the **directly submitted** task only:
`create_job_for_task_detailed` forwards the error without context
(`job_creator.rs:73`) and `execute_task` classifies it (`tasks.rs:540`). A
chain `A → B → missing C` returns 200 with a job id; `B`'s creation error is
caught at dispatch and fails `A`'s step (§ 3.3 step 2). The pre-check is one
level deep by design — it checks what this job's own steps reference.

`has_workspace` ignores `load_errors` (`workspace/mod.rs:519-527`); a
workspace whose *source construction* failed reads as unknown (400) here,
exactly as for action references. Not changed by this design.

### 3.2 Creation-time pre-check

In the step loop of `create_job_for_task_inner`, after the action is
resolved (`job_creator.rs:373`) and before `build_step`: if
`action.action_type == "task"`, call `resolve_task_ref` with base =
`(action_workspace, owner_cfg)` when the action is cross-workspace, else
`(workspace_name, workspace_config)`; then apply the self-reference row of
the table above; discard the result. It is an existence check so a bad
reference is a 400 at submit time rather than a failed step later. It runs
regardless of `when` — a missing task is a config error, unlike a
possibly-unused connection (`precheck_literal_connection_inputs` skips
`when`-guarded steps, `job_creator.rs:625-632`; this check does not). It
does **not** run in `build_step`: hook jobs also go through `build_step`,
and hook actions are not resolved cross-workspace (§ 5).

Nothing new is stamped on the step row. `action_workspace` /
`action_revision` keep their meaning and are set exactly as today.

### 3.3 Dispatch

`settlement/dispatch.rs::handle_task_steps_pass`, per ready `type: task`
step. The existing local `task` there is the **parent's** `TaskDef`
(`dispatch.rs:103`); the child's definition is `resolved.task`.

1. **Owner.** `O = step.action_workspace.as_deref().unwrap_or(&job.workspace)`.
   `O_cfg` is `workspace_config` when `O == workspace_name`, else
   `workspaces.get_config(O)`; `None` → `fail_task_step` with `workspace
   '{O}' is not available`.
2. **Resolve.** `resolve_task_ref(workspaces, O, &O_cfg, action_spec.task)`
   → `resolved` (`T`, `T_cfg`, bare name); `Err` → `fail_task_step` with the
   message (owner dropped the task since creation). `MAX_TASK_DEPTH`
   (`dispatch.rs:136-137`) is checked before this, as today.
3. **Step input** renders in the **caller's** context (`Scope::ChildTaskInput`,
   `dispatch.rs:158-163`). Unchanged. The result is the **caller bucket**
   `C` (its keys are what the caller supplied).
4. **Action defaults** come from the **persisted** `action_spec.input`
   (the action was serialized whole into `action_spec` at creation,
   `job_creator.rs:397-408`), not from a live lookup of `step.action_name`
   in any actions map — this removes the caller/owner name collision of
   § 1 and the "wrapper edited after creation" drift (revision 2). They are
   merged with `merge_action_defaults(&C, &schema, &{secret: O_cfg.secrets})`
   (`template.rs:791-825`): only fields absent from `C` are filled and
   rendered, with `O`'s **live** secrets. The keys it adds are the
   **action-default bucket** `D`. No connection resolution happens against
   the action schema any more (today `dispatch.rs:210` resolves it; § 8).
5. **Connection resolution against the task's schema, by provenance.** A
   new pure helper in `stroem-common/src/template.rs`:

   ```rust
   pub fn resolve_task_input_by_provenance(
       caller_input: &Value,    // C
       action_defaults: &Value, // D (disjoint keys)
       task_schema: &HashMap<String, InputFieldDef>,
       lookup: &dyn WorkspaceLookup,
       caller_ws: &str,         // A
       action_ws: &str,         // O
       task_ws: &str,           // T
   ) -> Result<Value>
   ```

   runs `resolve_connection_inputs_scoped` (`template.rs:580`) twice with
   the **task's** schema (`resolved.task.input`), once per bucket, and
   merges: `C` with `ResolveScope { schema_ws: T, value_ws: A, fallback_ws:
   Some(T) if A ≠ T }`; `D` with `{ schema_ws: T, value_ws: O, fallback_ws:
   Some(T) if O ≠ T }`. A bare name resolves first in the workspace whose
   YAML wrote it, then in `T` only if the connection is `shared: true`
   (`resolve_connection_ref`, `:247-320`); a qualified `ws.conn` goes
   through the ordinary gate. Fields the task's own defaults will fill are
   absent from both buckets and are left to the creator (step 6). Because
   nothing before this pass produces objects, every object reaching the
   creator has passed the task schema's canonical-type check (`:611-633`);
   the pass-through at `:600-602` no longer hides a type mismatch. When
   `A == O == T` both scopes are the local ungated scope — same-workspace
   behaviour for declared fields is unchanged. An error → `fail_task_step`.
   `lookup` is `WorkspaceSet::load(workspaces, T, Some(&resolved.config))`
   (`workspace_set.rs:26-34`): every loaded config is in the set and
   `local_override` pins `T` to the same snapshot step 2 resolved against
   (`:78-83`).
6. **Create** with the owner's snapshot:
   `create_job_for_task_inner(workspaces, pool, &resolved.config, &T,
   &resolved.task_name, input.clone(), "task", Some(&source_id),
   ParentLink::TaskStep { job_id, step_name, input: &input }, revision,
   CreationMode::Normal, None, defaults)`. Inside, `merge_defaults` fills
   the task's own defaults with `T`'s secrets (`job_creator.rs:281-283`) and
   `resolve_connection_inputs` resolves them ungated in `T` (`:315-317`) —
   the owner reading its own config. `raw_input` is the input as passed
   (`:278`). The parent-step claim (§ 4.1) happens inside this call's
   transaction.
7. **Revision.** `revision = if T == job.workspace { job.revision } else {
   workspaces.get_revision(&T) }` (`workspace/mod.rs:473`). Same-workspace
   children keep inheriting the parent's revision; a foreign child gets the
   owner's revision at the moment it is created. **Consistency contract:**
   the config snapshot (`resolved.config`, taken once in step 2 and reused
   in steps 5–6) and the revision are separate reads; `do_reload` loads
   the source (revision advances) before swapping the config
   (`workspace/mod.rs:664-680`), so a reload between the two reads pairs a
   v1 config with a v2 revision — steps built from v1 run against the v2
   tarball. This is the same exposure a top-level execute has today
   (`create_job_for_task` reads config and revision separately) and is not
   widened by this design; closing it needs `WorkspaceManager` to hand out
   `(config, revision)` under one lock (TODO.md).
8. **Log.** The `tracing::info!` at `dispatch.rs:279` and the parent job's
   creation line gain ` in workspace '{T}'` when `T ≠ A`.

The child row is stamped `workspace = T`, `task_name = bare`,
`parent_job_id`, `parent_step_name`, `source_type = "task"` — the last three
as today (`dispatch.rs:266-269`).

**Error scrubbing.** `fail_task_step` scrubs with the caller config's
secrets only (`dispatch.rs:77-78`, `collect_config_secret_values`). A Tera
error from step 4 (an action default rendering `{{ secret.TOKEN | round }}`
in `O`) or from the creator (a task default in `T`) quotes the offending
value. `handle_task_steps_pass` builds the scrub list once per step as
`collect_config_secret_values(A_cfg) ++ (O_cfg) ++ (T_cfg)` (concatenation,
`workspace_set.rs:103-109`) — `T_cfg` only once step 2 has it — and
`fail_task_step` takes the list instead of a config. Scrubbing at the write
covers the job log, `job_step.error_message`, `retry_history`, and every
reader including MCP `get_job_status`, which returns the persisted text
without the REST redactor (`mcp/tools.rs:570-580`). Inherited limits stay:
values of ≤ 3 characters are not scrubbed (`workspace_set.rs:169`) and a
secret rotated out of the loaded config is not either.

### 3.4 What does not change — verified

Consumers downstream of creation key off the **child row's own**
`workspace` / `task_name`:

- **Propagation** — `Settlement::propagate` writes the parent step from
  `child_job.status` / `.output` and calls `advance(parent_job_id)`
  (`propagate.rs:135-150`); `advance` re-reads the parent row and resolves
  *its* workspace (`settlement/mod.rs:201`, `:135-136`). The parent-step
  write itself changes (§ 4.3), not where it looks.
- **Terminal handling, hooks** — resolved from the terminal job's own row
  (`settlement/mod.rs:135-169`). A child is not a top-level source
  (`hooks.rs:78-83`), so only `T`'s *task-level* `on_*` hooks fire, in `T`
  (`hooks.rs:284-293`). `on_suspended` for a child suspended at creation is
  fixed in § 4.4.
- **ACL** — no per-child check exists today: `execute_task` checks `Run` on
  the top-level task only (`web/api/tasks.rs:435-477`) and dispatch never
  consults ACL. Unchanged (decision, revision 1). Job detail / logs /
  artifacts check `View` against the row's `(workspace, task_name)`
  (`web/api/jobs.rs:1141-1161`), so the child is governed by `T`'s rules;
  on Deny the detail endpoint answers **404** (`jobs.rs:302-304`), and
  "no rule" means the ACL's configured `default`. Cancelling `A`'s job
  recursively cancels the child with `A`'s authorization only
  (`settlement/mod.rs:748-753`) — a caller may stop what it started.
- **Log archive key** (`terminal.rs:132-138`), **state snapshots**
  (`settlement/mod.rs:215-221` — the child reads `T/deploy`'s state; the
  step input in `A` renders with `A`'s snapshots), **artifacts**, **restart
  / re-run rejection** (`is_top_level_job`), **`compute_depth`**
  (`job_creator.rs:600`), **reconcile** (`job.rs:814-845`, no
  workspace-equality predicate), **loop expansion** (copies
  `action_workspace` / `action_revision`).
- **Redaction** of the child's detail covers every loaded workspace's
  secrets (`workspace_set::collect_redaction_values`).

### 3.5 Cycles

`check_task_self_reference` (`validation.rs:1048-1069`) compares bare names
within one config and cannot see `task: A.p` inside `A`; the creation
pre-check catches that direct form (§ 3.1 table). A cross-workspace cycle
(`A/p → B/q → A/p`) is bounded by `MAX_TASK_DEPTH` (10) like any indirect
recursion today; ancestry-based cycle detection is deliberately **not**
added, because bounded indirect recursion with a `when` guard is a
legitimate pattern that it would break. Documented.

### 3.6 Trust model

**Calling a task is delegation to its author.** Any workspace may call any
task (open, like actions); the top-level `Run` check on the caller's task
is the authorization; there is no per-child check. The task author in `T`
must therefore write the task as if any workspace may run it with any
input its schema accepts — the same assumption a webhook-triggered task
already lives under:

- The provenance pass (§ 3.3 step 5) governs **connection-typed inputs
  declared on the task**. It is hygiene, not isolation: a *primitive*
  input that the task templates into a connection name on one of its own
  actions binds in `T`'s own scope with no `shared` gate
  (`template.rs:258-265`, worker `rendering.rs:125-129`) — exactly as when
  `T`'s own trigger runs it. Undeclared extra fields pass through
  `merge_defaults` untouched (`template.rs:509`); required fields are not
  enforced at creation (`:536-538`).
- What crosses back to `A`: the child's terminal status and its **output,
  verbatim** (`propagate.rs:135-141`) — deliberate disclosure, the caller
  asked for the result. A task that emits credentials in its output
  discloses them to every caller, as it does to its own hooks today.
- What never crosses: `T`'s secrets and config are not rendered in `A`'s
  context; the child's detail is `T`-ACL'd and redacted; render errors are
  scrubbed with all three configs' secrets (§ 3.3).
- A child is **not** the owner's scheduler run: it takes no task-level
  retry (`terminal::plan` gates on `parent_job_id.is_none()`,
  `terminal.rs:180-183`), fires no workspace-level hooks, and is cancelled
  by the caller's cancellation. Established child semantics, documented.

### 3.7 Parent → child link

The job detail step DTO (`web/api/jobs.rs:259-`) exposes no child job id;
a child's only pointer is its own `source_id = "{parent}/{step}"`
(`dispatch.rs:252`), and the UI renders it as text (`job-detail.tsx:348`).
For a same-workspace child the user can find it in the same list; a
cross-team child is in `T`'s list only. The step DTO gains
`child_job: Option<{ id, workspace, task_name, status }>` for `type: task`
steps, filled by the new `JobRepo::get_child_job_for_step(parent, step)`
(also used by § 4.3), and the Job Detail step row renders it as a link to
`/jobs/{id}`. The link follows the child's own ACL (404 if denied).

## 4. Lifecycle hardening

Revision 1 claimed the dispatch → child → propagate path was "handled
correctly with no code change". It is not, for any child; a foreign child
makes each gap a cross-team contract failure, so all three are fixed here.

### 4.1 Exclusive, crash-safe dispatch (F4a, F4c)

Today dispatch persists the rendered input (`dispatch.rs:240`), marks the
step running with a guarded UPDATE whose `rows_affected` is discarded
(`job_step.rs:630-645`), marks the job running (`:250`), then creates the
child in its own transaction (`job_creator.rs:411-460`). Two `advance`
calls on the same job can both pass the `status == ready` read
(`dispatch.rs:122`) and both create a child; a crash between the mark and
the commit leaves a `running` step with no child and no worker, which
nothing repairs (recovery's phase 2 needs a `timeout_secs`,
`job_step.rs:1083-1093`).

The parent-step transition moves **inside the child-creation transaction**.
`create_job_for_task_inner`'s `parent_job_id` / `parent_step_name` pair
becomes

```rust
pub(crate) enum ParentLink<'a> {
    None,
    AgentTool { job_id: Uuid, step_name: &'a str }, // worker-owned parent step, no claim
    TaskStep  { job_id: Uuid, step_name: &'a str, input: &'a Value }, // server-owned, claimed here
}
```

(callers: `create_job_for_task_detailed` and the restart creator → `None`;
`create_child_job_for_task_detailed` → `AgentTool`, its parent step stays
`running` under a worker, `worker_api/jobs.rs:1112`; dispatch →
`TaskStep`). For `TaskStep`, first inside `tx`:

```sql
SELECT 1 FROM job WHERE id = $parent AND status IN ('pending','running') FOR SHARE;
UPDATE job_step SET status = 'running', started_at = NOW(), input = $input
 WHERE job_id = $parent AND step_name = $step AND status = 'ready';
UPDATE job SET status = 'running' WHERE id = $parent AND status = 'pending';
```

as `JobStepRepo::claim_task_step_tx(...) -> Result<bool>`. If the `SELECT`
returns no row or the step `UPDATE` affects 0 rows, the creator returns
`Err(DispatchLost)` (a typed error, `job_creator::DispatchLost`) and the
transaction rolls back — no child. Dispatch matches on it: log at `debug`
and `continue`; it is **not** a step failure (another settlement, or a
cancellation, owns the step). Then the job and steps are inserted and the
transaction commits: the step is `running` iff its child exists. A crash
before commit leaves the step `ready` and the next `advance` re-dispatches
it; a crash after commit is the ordinary "child exists, its `init` did not
run" exposure every job creation already has (`job_creator.rs:486-508`;
root steps without `when` / deps start `ready` and are claimed regardless,
`:389-390`) — inherited, TODO.md.

`update_input` (`dispatch.rs:239-244`) and `mark_running_server` /
`mark_running_if_pending_server` (`:247`, `:250`) leave the dispatch path;
`mark_running_server` stays for approval steps (`:315`).

### 4.2 Cancellation during dispatch (F4b)

`Settlement::cancel` stamps the job (`JobRepo::cancel`, `WHERE status IN
('pending','running')`, `job.rs:754-767`) and its steps, **then**
enumerates active children (`get_child_jobs`, `settlement/mod.rs:748`) and
recurses. Without a lock, a child committed between the stamp and the
enumeration is never cancelled. The `FOR SHARE` in § 4.1 serialises the two:
`JobRepo::cancel`'s `UPDATE` takes `FOR NO KEY UPDATE` on the job row, which
conflicts with `FOR SHARE`, so either the claim waits for the cancel to
commit and then sees `cancelled` (no child), or the cancel waits for the
claim's transaction to commit and its enumeration then sees the child.
The claimed step is `running` with `worker_id IS NULL`, which
`cancel_server_managed_steps` already stamps (`job_step.rs:649-662`).

### 4.3 Timed-out parent step and late child completion (F4d)

Recovery phase 2 fails a `type: task` step whose `timeout_secs` elapsed
(`recovery.rs:117-159`; the query includes worker-less steps) and leaves the
child running; when the child finishes, `propagate` writes the parent step
with the unguarded `mark_completed` / `mark_failed` (`propagate.rs:135-148`,
`job_step.rs:665-710`), turning a `failed` step back into `completed` and
re-cascading a job that already settled.

- **Guarded propagation.** `propagate`'s three parent-step writes require
  `status = 'running'` (`mark_completed_if_running`, `mark_failed_if_running`
  — new, returning `bool`; `mark_cancelled` already is guarded,
  `:1042-1057`). On 0 rows the child's result is logged to the parent job
  (`[task] child {id} finished {status} after step '{step}' was already
  {status} — result discarded`) and `advance(parent)` still runs (it is
  idempotent; the parent is usually terminal and the claim was consumed).
- **Timeout cancels the child.** After `step_failed` for a timed-out
  `type: task` step, recovery calls `Settlement::cancel` on
  `get_child_job_for_step(parent, step)` when it is active (new query; a
  `for_each` instance's `parent_step_name` is `step[i]`). The child's own
  cancellation then propagates and hits the guard above. Job-level timeout
  (phase 3, `:205-217`) already goes through `cancel`, which recurses.

### 4.4 Child suspended at creation fires `on_suspended` (F6)

`dispatch::init` (pool tier, `dispatch.rs:600-648`) suspends root approval
steps via `handle_approval_steps` (`:315`), which cannot fire hooks; the
sweep that does, `fire_initial_suspended_hooks` (state tier, `:489-565`), is
called by the six top-level creation entry points (`scheduler.rs:439`,
`web/hooks.rs:135`, `worker_api/event_source.rs:121`, `web/api/tasks.rs:546`,
`web/api/jobs.rs:806`, `mcp/tools.rs:530`) and by no child path
(`dispatch.rs:277-285` drops `CreatedJob`). `advance`'s `dispatch_approvals`
fires only for steps that *became* suspended in that call (`mod.rs:360`).

One owner: `Settlement::job_created` (`mod.rs:520-530`) and
`agent_child_created` (`:534-555`) fire the initial suspended hooks for the
created job **and every descendant created during its `init`**. To carry
the ids up: `handle_task_steps_pass` collects `created.job_id` plus
`created.descendants` (a new private `Vec<Uuid>` on `CreatedJob`, the
transitive list from the child's own `init`); `handle_task_steps` returns
the list; `init` stores it on the `CreatedJob` it returns; `advance` (state
tier, `mod.rs:227-236`) fires for the ids `handle_task_steps` returns to it
directly. The six explicit `fire_initial_suspended_hooks` calls are removed
— `job_created` is already the mandatory last step of every creation site
(CLAUDE.md § Settlement, `CreatedJob` obligation), so the hook sweep now
rides on that obligation instead of on six call sites. `fire_initial_
suspended_hooks` itself is unchanged; a per-job wrapper
`Settlement::fire_initial_suspended_hooks_for(job_id)` loads the row and
resolves its workspace/task (`resolve`, `mod.rs:135`; unresolvable → skip,
logged).

### 4.5 Task removed while the job runs (F5)

`Settlement::resolve` returns `Ok(None)` both when the workspace config is
unavailable (`mod.rs:136-152`) and when the config is loaded but the task
is gone (`:160-167`); for a non-terminal job `advance` returns early
(`:209-211`) and nothing re-enters it later — no sweep touches a job whose
steps are all terminal but whose row is `running`. A workspace reload that
drops a task therefore strands every running job of that task, and the
caller's parent step waits forever.

`resolve` returns a three-way `Resolution { Resolved(ws, task) |
Unavailable | TaskGone }` (hook / event-source sources keep
`build_minimal_task_def`, `:155-159`). For a non-terminal job:

- `Unavailable` — early return as today (a transient reload failure must
  not fail jobs). That a job can then stall until its next step event is
  inherited and tracked: "re-advance running jobs of a workspace after a
  successful reload" (TODO.md).
- `TaskGone` — the job is failed, in one transaction: every non-terminal
  step (`pending`, `ready`, `suspended`, `running`, `claimed`) is set
  `failed` with `error_message = "[settlement] task '{ws}/{task}' no longer
  exists"` and the job row is settled `failed` (`JobRepo::settle`); the
  cancel signal is published for the job id so live workers abort
  (`cancelled_jobs` + NOTIFY, as `cancel` does at `mod.rs:718-742`); the
  line is appended to the job log. `advance` then continues into its
  terminal branch in the same call: drain passes (no live steps), claim,
  `propagate` → the parent step fails with `Child job … failed` and `A`
  cascades; hooks and archive are skipped as today for an unresolvable
  task (`:303-305`), but the log is still closed. A worker completing a
  step afterwards writes `completed` over `failed` on that row (unguarded,
  inherited); the job row is already terminal and `settle` will not
  reopen it.

### 4.6 Carried risks (unchanged by this design)

- `init` not running after a committed creation (crash) — § 4.1.
- A job under an `Unavailable` workspace stalls until its next step event
  — § 4.5.
- Config/revision read separately — § 3.3 step 7.
- Worker completion writes are unguarded against a failed step — § 4.5.

## 5. Validation (`stroem-common`)

`validate_workflow_config_inner`'s `type: task` check (`validation.rs:65-78`):

- CLI path (`libraries_resolved == false`, `local/validate.rs:21`): a dotted
  `task:` that is not a local key is skipped **with a warning** (`cannot
  validate cross-workspace task reference '…' offline`) — today it is
  skipped silently as a presumed library name (`:69-70`). A dotted name
  with an empty side (`.deploy`, `B.`) is an error in both modes.
- Server path: `CrossWorkspaceActionResolver` (`validation.rs:19`, `&dyn
  Fn(&str, &str) -> bool`) becomes a trait `CrossWorkspaceResolver { fn
  has_action(&self, ws, name) -> bool; fn has_task(&self, ws, name) ->
  bool }`; the existing closure call sites become `has_action`. A dotted
  `task:` that misses locally is accepted iff `has_task(ws, name)`; with no
  resolver (`validate_workflow_config_with_libraries`) it is an error, as
  today. The resolver validator is still not wired into any server load
  path (pre-existing, CLAUDE.md); § 3.2 is the enforcement.
- **Hooks.** A `type: task` action whose `task:` is not a local key is
  rejected when it is referenced from any `on_*` hook (task-level or
  workspace-level, `validate_hook_action_exists` sites,
  `validation.rs:1820-1834`): `hook '{label}' uses action '{a}' whose task
  '{ref}' is in another workspace; hook actions cannot call tasks across
  workspaces`. The runtime path (`hooks.rs:566-575`, which hands the raw
  name to the local creator) bails with the same wording, so an unvalidated
  config fails the hook job clearly instead of with `not found`.
- `check_task_self_reference` stays bare-name; the qualified direct form is
  caught at creation (§ 3.1).

## 6. Tests

Each pins one seam. The multi-workspace fixture
(`setup_multi_workspace_with`, `integration_test.rs:8973-9095`) has both
workspaces at `"test-rev"` with `auth: None`, `acl: None`, and an
`InMemSource` that reloads to the same config; it gains an options struct
(`MultiWsOpts { revisions: (&str, &str), acl: Option<AclConfig>, auth:
bool }`) and the in-memory source becomes `Arc<Mutex<WorkspaceConfig>>` so
a test can change a workspace and `mgr.reload(name)`. A third workspace
`C` is added for the three-owner cases.

- **`stroem-common` unit** — `resolve_task_input_by_provenance`: caller
  value found in `A`; caller bare name absent in `A`, present-and-shared in
  `T`; present-but-unshared in `T` → `is not shared`; action default naming
  `O`'s own private connection resolves ungated in `O`; action default
  naming `T`'s connection → shared gate; canonical-type mismatch between
  the connection's declared type and the task field's type is an error
  (the object no longer bypasses it); `A == O == T` equals the local
  result. Validation: dotted `task:` CLI skip + warning; server accept /
  reject via `has_task`; local flattened key wins over the split; empty
  side rejected; hook with a qualified task rejected.
- **`job_creator` unit** — `resolve_task_ref`: local hit, library-flattened
  hit, qualified hit, unknown workspace, unavailable workspace,
  loaded-but-no-task, direct self-reference `A.p` inside `A`; each error's
  exact phrase. `classify_execute_error`: `"has no task"` → 400.
- **Integration, resolution** — Form A creates a child with `workspace =
  B`, `task_name = "deploy"`, `revision = B`'s (≠ `A`'s); completing it
  propagates output to the parent step and the parent completes. Form B
  does the same, and its defaults come from the persisted `action_spec`
  even after `B`'s `run-deploy` is edited and reloaded. Form B whose
  owner's `task:` names `C` (`A ≠ O ≠ T`) resolves relative to `B`. **Name
  collision regression (F8):** `A` has its own `deploy`; `action:
  B.run-deploy` now runs `B/deploy`, and the parent's `[task]` log line
  names `B`. Same-workspace child still inherits the parent's revision.
- **Integration, errors** — execute: `task: nope.deploy` → 400, `task:
  B.nope` → 400, `B` a placeholder → 500, `task: A.p` inside `p` → 400;
  `A → B → missing C` → 200 and `A`'s step fails with `B`'s message. Owner
  task removed **before** dispatch → step fails, dependents skip, job
  fails. A default in `B` rendering `{{ secret.TOKEN | round }}` with a
  non-numeric token: the persisted step error, the job log, and MCP
  `get_job_status` all show `••••••`, not the token.
- **Integration, lifecycle** — (§ 4.1) two concurrent `advance` calls on a
  job with one ready `type: task` step create exactly one child, and the
  loser logs `DispatchLost`; a creator that errors after the claim leaves
  the step `ready` (rollback). (§ 4.2) `cancel` racing dispatch: either no
  child exists or the child is `cancelled` — never a running orphan (loop
  the interleaving under a barrier). (§ 4.3) parent step with
  `timeout: 1s`, child running: recovery fails the step **and** the child
  is cancelled; a child completing after its parent step was failed leaves
  the step `failed` and logs the discard line. (§ 4.4) `B/deploy` whose
  root is an approval step: `on_suspended` fires once, in `B`, for a child
  created by dispatch, for a grandchild created during `init`, for a
  top-level job (regression for the removed six calls), and for an
  agent-tool child; never twice. (§ 4.5) `B` reloaded without `deploy`
  while the child runs and a worker then completes the last step: the
  child is `failed` with the `[settlement]` line, the parent step is
  `failed`, `A` closes; `B` unavailable (placeholder) instead → the child
  stays `running` (documented stall), and recovers once `B` reloads and
  the next step event arrives.
- **Integration, access** — with `MultiWsOpts { acl: Deny-by-default +
  Run on A/pipeline }`: 200 on the parent, **404** on the child detail and
  logs; admin sees both; the parent's step DTO carries `child_job` and the
  link target 404s for the restricted user.
- **Integration, hooks** — `B/deploy`'s `on_error` fires in `B`; `A`'s
  workspace-level `on_error` does not fire for the child; `B`'s task-level
  `retry` does not create a retry job for the child (documented).
- **`tests/e2e.sh`** — one cross-workspace `type: task` step, server ↔
  worker: `B`'s step reads a file that exists only in `B`'s tarball and a
  `{{ secret.* }}` that exists only in `B`, and its output round-trips to
  `A`'s parent step.

## 7. Documentation

- `docs/src/content/docs/guides/cross-workspace-references.md`: new section
  "Calling tasks in other workspaces" — both forms, the `A / O / T` rule and
  precedence, "input renders in the caller, defaults come from the action's
  owner, the child runs as the task's owner", the connection provenance
  table, the revision rule and how it differs from actions (§ 3.3 step 7),
  the trust model (§ 3.6) verbatim in user terms, ACL and visibility (404),
  hooks / retry / cancel semantics of a child, the depth bound for cycles,
  the parent → child link; remove the first "Not yet supported" bullet
  (`:245`) and state that hook actions still cannot.
- `docs/src/content/docs/guides/action-types.md` `type: task` section: the
  behaviour corrections in § 8.
- `CLAUDE.md` § Cross-Workspace References: replace the deferred bullet with
  the `A / O / T` rule and `resolve_task_ref`; § Task Actions: the
  transactional claim, `ParentLink`, `DispatchLost`; § Settlement: `job_created`
  owns initial `on_suspended` hooks, `Resolution::TaskGone` fails the job,
  guarded propagation; § Worker Recovery: phase 2 cancels a timed-out task
  step's child. `CONTEXT.md`: "Owner workspace (action owner / task owner)",
  "Dispatch claim".
- `docs/internal/TODO.md`: add the four carried risks (§ 4.6) and
  "ancestry-based cycle detection" (rejected for now, § 3.5); tick the
  cross-workspace `type: task` item if present.

## 8. Behaviour corrections (release notes)

1. `action: B.run-deploy` (a cross-workspace `type: task` action) now runs
   `B`'s task. Before, it ran the **caller's** task of the same bare name if
   one existed, else failed.
2. A `type: task` action's own `input` defaults are taken from the
   definition persisted on the step at job creation, not re-read at
   dispatch; editing the action between creation and dispatch no longer
   changes an in-flight job's child.
3. Connection-typed fields on a `type: task` **action's** schema are no
   longer resolved against the action schema; resolution happens once,
   against the **task's** schema. A field the action declares as a
   connection but the task declares as a primitive now arrives as the name
   string, not the resolved object.
4. `on_suspended` hooks now fire for a child job (any workspace) whose root
   step is an approval; previously only top-level jobs got them.
5. A running job whose task is removed from its workspace is now failed
   with `[settlement] task '…' no longer exists` instead of staying
   `running` indefinitely.
6. A timed-out `type: task` step now cancels its child job, and a child
   finishing after its parent step was failed or cancelled no longer
   overwrites that status.

## 9. Out of scope

Cross-workspace hook actions and cross-workspace `agent` steps rendering
against the owner (both still deferred); a `shared:` flag on tasks
(decided against); per-child ACL (decided against); pinning the owner
revision at parent creation (rejected, § 2); ancestry-based cycle detection
(§ 3.5); atomic `(config, revision)` reads and re-advancing stalled jobs on
reload (§ 4.6); event-source backpressure counting across workspaces.
