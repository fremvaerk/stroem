# Cross-Workspace `type: task` Actions — Design

Status: revision 5, reviewed — ready for an implementation plan
Ships in: 0.17.0 (minor; no migration; documented behaviour corrections, § 7)

Closes the first item under "Deferred" in CLAUDE.md § Cross-Workspace
References and "Not yet supported" in
`docs/src/content/docs/guides/cross-workspace-references.md:245`. Line numbers
cite `main` at `f174020`.

## Revision history

**Revision 6 (2026-09-16, implementation).** Implemented as designed, with
one seam differing from § 3.7: the parent→child link is
`JobRepo::list_children(parent)` (`crates/stroem-db/src/repos/job.rs`) —
every child job of the parent, `ORDER BY created_at DESC, job_id DESC` —
fetched once per job detail request and grouped in memory by
`parent_step_name`, rather than a `get_child_jobs_for_step(parent, step)`
call per step. The step DTO field is `child_jobs`, populated for `type:
task` steps only, each entry `{ id, workspace, task_name, status,
created_at }`: execution history newest-first, with no attempt identity, as
specified.

**Revision 5 (2026-09-16).** Revision 4's review found no design blocker
("proceed with implementation planning") and two wording defects, fixed:
§ 7 item 4 now states the exact conditions for a submit-time 400
(unguarded, caller-supplied literal, foreign bucket) and where every other
case fails; the § 6 guarded-step test uses a true guard (a skipped step
does not fail); the crossing-overlap example in § 3.3 names the order it
describes. The mask edge cases the review listed (values containing `•`
or equal to a substring of the mask, empty values, multi-byte boundaries,
and that a non-overlapping matcher does not satisfy the definition) are
added to § 6.

**Revision 4 (2026-09-16).** Revision 3's review gave a conditional pass on
the feature and found five contained defects, all fixed here. Longest-first
sequential replacement is not a complete scrub: two secrets whose
occurrences *cross* (`incorrect value: got "ABCD` and `ABCD-token`, the
exact shape Tera produces, `workspace_set.rs:117-118`) leave the owner
secret's suffix legible, and a self-overlapping value (`aba` in `ababa`)
is never fully masked by non-overlapping replacement — § 3.3 now masks the
union of every match span found in the *original* text. The creation
pre-check selected only literal *strings*, so the promised submit-time 400
for a literal object was actually a 200 with a failed step — § 3.2 now
includes the shape check, keeps the `when` exemption, and wraps the error
in a classifier-recognised context. The child list cannot identify the
"current attempt" (a step retry resets the row before any replacement
child exists, `job_step.rs:768-779`; duplicate dispatch creates two
children for one attempt) — § 3.7 is execution history, newest first, with
a deterministic secondary sort. Only *objects* pass through the local
resolver (`template.rs:600-606`; arrays and scalars already fail), and the
boundary rule is per bucket, not only for `A == O == T` — § 3.3 step 5 and
§ 6 say so. The pre-check's lookup pins `T` to `resolved.config`, like
dispatch. § 4's wording is qualified where it overclaimed (job timeout also
ends a stranded job; a stale dispatcher can fail a winner's step; the
terminal claim and propagation are not atomic), and § 3.6's "cancelled by
the caller's cancellation" carries the enumeration-race caveat. The
lifecycle problem statement gains a seventh paragraph (terminal claim →
propagation window), required outcomes for committed-child-without-init,
compensation failure and `retry_at`, the agent barrier as a requirement,
and the lock-order caveat against the cascade spec's `cancel` contract.

**Revision 3 (2026-09-16). Scope cut.** Revision 2's review (Codex, same
thread) closed F1, F7, F8 and F10 and found thirteen new findings, seven
P1. Five of the P1s and three of the P2s were about the lifecycle fixes
revision 2 had pulled in for F4/F5/F6: the `FOR SHARE` claim admits a
lock-upgrade deadlock between two dispatchers (Postgres aborts a victim, it
does not return `DispatchLost`; `cascade.rs:938-941` retries `40P01`, the
creator does not); pre-claim failure paths (`fail_task_step`,
`dispatch.rs:81`) are unfenced, so a stale dispatcher can fail the winner's
step; propagation guarded on status alone lets a stale child settle a
*replacement attempt* after a step retry (`job_step.rs:754-789`);
`TaskGone` fabricated a drained job — the drain gate reads statuses, not
process termination (`job_step.rs:1012-1017`), `advance` then clears the
cancel signal (`settlement/mod.rs:266`) while workers still poll it
(`worker_api/jobs.rs:1045-1050`), a late failure with retries left re-readies
the step (`job_step.rs:754-789`), and descendants were never cancelled; the
hook move transported job ids without owning transitions, so the initial
sweep (`dispatch.rs:536-539`) and `advance`'s snapshot diff
(`settlement/mod.rs:369-404`) could each fire for the same step. Each of
those is a design of its own, none is made worse by a cross-workspace
child, and every one applies to same-workspace children today.

Decision: **split.** This design ships the feature and carries the
lifecycle gaps as documented risks (§ 4); their problem statement, with
revision 2's review as input, is
`2026-09-16-task-step-lifecycle-hardening-design.md`. The contained findings
are fixed here: values crossing a workspace boundary into a
connection-typed task field must be names, not objects (§ 3.3 step 5);
secrets are scrubbed longest-first (§ 3.3); for a `type: task` action the
creation pre-check runs against the *task* schema (§ 3.2); the
hook-validation rejection is server-side only, the CLI warns (§ 5); the
parent→child link is a list (§ 3.7); and every citation
in revision 2's audit table is corrected (`rendering.rs:134`,
`job_creator.rs:542`, `jobs.rs:304-305`, `terminal.rs:185-187`,
`validation.rs:1824` + call sites, `collect_config_secret_values` is
unfiltered). The unsupported "as it does to its own hooks" comparison and
the claim that ready roots are "claimed regardless" (`job_step.rs:570`
excludes server-managed kinds) are removed.

**Revision 2 (2026-09-16).** Revision 1's review found ten defects, all
verified against the code. Four were design defects: the connection pass
could not deliver the isolation § 3.6 promised (`template.rs:258-265`,
`rendering.rs:134`); the pipeline lost provenance between the action's
defaults and the task's schema (`template.rs:600-602`); § 1's "always fails"
premise was false — a caller task with the same bare name runs *today*
(`job_creator.rs:213`); and `task: A.p` inside `A` slipped past
self-reference validation (`validation.rs:1054`). Three were overclaims
("cannot read B's secrets"; "owner's current config and revision" is not
one snapshot; a child is not the owner's scheduler run). Three were the
inherited lifecycle gaps now in § 4. Decisions: open access stated as
trusted delegation (§ 3.6); action defaults from the persisted
`action_spec` (§ 3.3 step 4); job detail returns 404 on Deny; a
parent→child link (§ 3.7).

**Revision 1 (2026-09-16).** Initial design. Decisions from the brainstorm:
cross-team pipelines are the driving use case; access is open, like
cross-workspace actions; both reference forms; no child-level ACL check;
resolve at dispatch with no schema change.

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
  column and calls `create_job_for_task_inner` with the **caller's** config
  and the bare name `deploy` (`dispatch.rs:259-264`). If the caller has no
  task `deploy` the step fails; **if it has one, the caller's `deploy`
  runs** — the wrong task, silently. The action's input defaults are
  likewise merged from the caller's actions map by bare name
  (`dispatch.rs:206`).

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

1. `O_cfg.tasks.get(task_ref)` — a hit means `T = O`. Library-flattened
   names (`common.deploy`, inserted whole at `workspace/library.rs:339`)
   land here, keeping the "library first, workspace on miss" precedence
   that action references use (`job_creator.rs:328-334`). A library action's
   own `task:` was prefixed at import (`library.rs:345-347`) and is a whole
   key of the importing workspace — never split.
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
    pub task: TaskDef,                // resolved.config.tasks[task_name], cloned
}
pub(crate) async fn resolve_task_ref(
    workspaces: &WorkspaceManager,
    base_ws: &str,                    // O
    base_cfg: &Arc<WorkspaceConfig>,
    task_ref: &str,
) -> Result<ResolvedTask>;
```

The self-reference check is **not** the resolver's (it has no caller
identity); it is the creation pre-check's (§ 3.2).

Errors are `anyhow!` with no further `.context()` so they stay the
**outermost** message (`classify_execute_error` matches the legacy phrases
on the outermost message only, `web/api/mod.rs:404-411`):

| Condition | Message | HTTP at creation |
|---|---|---|
| dotted, `ws` not configured (or a library prefix that does not exist) | `task '{ref}': unknown workspace '{ws}'` | 400 (`"unknown workspace"`, `:398`) |
| `ws` configured, `get_config` is `None` | `task '{ref}': workspace '{ws}' is not available` | 500 (`:394`) |
| `ws` loaded, no such task | `task '{ref}': workspace '{ws}' has no task '{name}'` | 400 — **new** phrase `"has no task"` beside `"has no action"` (`:407`) |
| resolves to the task being created (`T == A && name == task_name`, checked in § 3.2) | `task '{ref}' is a self-reference to '{A}/{task}' (invalid)` | 400 (`"invalid"`, `:409`) |
| not dotted, missing | unchanged (`job_creator.rs:213-218`) | 400 (`"not found"`) |

The HTTP outcome applies to the **directly submitted** task only:
`create_job_for_task_detailed` forwards the error without context
(`job_creator.rs:73-92`) and `execute_task` classifies it (`tasks.rs:540-541`).
A chain `A → B → missing C` returns 200 with a job id; `B`'s creation error
is caught at dispatch (`dispatch.rs:286-300`) and fails `A`'s step. The
pre-check is one level deep by design.

`has_workspace` ignores `load_errors` (`workspace/mod.rs:519-527`); a
workspace whose *source construction* failed reads as unknown (400) here,
exactly as for action references. Not changed by this design.

### 3.2 Creation-time pre-check

In the step loop of `create_job_for_task_inner`, after the action is
resolved (`job_creator.rs:373`) and before `build_step`, for
`action.action_type == "task"`:

1. `resolve_task_ref` with base = `(action_workspace, owner_cfg)` when the
   action is cross-workspace, else `(workspace_name, workspace_config)`. A
   bad reference is a 400 at submit time rather than a failed step later.
   Runs regardless of `when` — a missing task is a config error.
2. If `resolved.workspace == workspace_name && resolved.task_name ==
   task_name` → the self-reference error (§ 3.1 table).
3. **Literal connection pre-check against the task schema.** Today
   `precheck_literal_connection_inputs` (`job_creator.rs:617-666`) checks
   the flow step's literal string values against the **action's** input
   schema. For a `type: task` action that contradicts § 3.3 step 5, where
   resolution happens against the **task's** schema (a wrapper declaring one
   connection type but forwarding into a task expecting another would be
   rejected at submit and accepted at dispatch). So for `type: task`
   actions the action-schema pre-check is **skipped** and replaced by one
   that mirrors step 5's caller bucket exactly. For each key of the flow
   step's `input:` that is a connection-typed field of
   `resolved.task.input` and whose value is **literal** (a string with no
   `{{`, or any non-string):
   - the **shape rule** of step 5 applies first — when `A ≠ T`, a
     non-string (object, array, number, bool, null) is refused with the
     same message;
   - a string is resolved with the caller scope (`schema_ws: T, value_ws:
     A, fallback_ws: Some(T) if A ≠ T`), through
     `WorkspaceSet::load(workspaces, T, Some(&resolved.config))` so the
     type definitions come from the snapshot the task was resolved
     against (the creator's own `ws_set`, `:315`, pins the caller).
   Both errors are wrapped in `.context("Failed to resolve connection
   inputs")` — the phrase the existing pre-check relies on (`:664`) and
   the classifier matches on the outermost message (`"resolve
   connection"`, `web/api/mod.rs:406`) — so they are 400, while an
   unavailable owner inside the chain still classifies as 500 (`:394`).
   The existing rule that a `when`-guarded step is not pre-checked at all
   stays (`:625-632`): for such a step a bad literal fails at dispatch if
   the step is reached, never at submit. Only caller-supplied literals are
   checked — action defaults and task defaults are the owners reading
   their own config. Templated values are checked at dispatch.

Nothing new is stamped on the step row. `action_workspace` /
`action_revision` keep their meaning and are set exactly as today; the
action is serialized whole into `action_spec` by `build_step`
(`job_creator.rs:542`).

### 3.3 Dispatch

`settlement/dispatch.rs::handle_task_steps_pass`, per ready `type: task`
step. The existing local `task` there is the **parent's** `TaskDef`
(`dispatch.rs:103`); the child's definition is `resolved.task`.

1. **Owner.** `O = step.action_workspace.as_deref().unwrap_or(&job.workspace)`.
   `O_cfg` is `workspace_config` when `O == workspace_name`, else
   `workspaces.get_config(O)`; `None` → `fail_task_step` with `workspace
   '{O}' is not available`.
2. **Resolve.** `resolve_task_ref(workspaces, O, &O_cfg, action_spec.task)`
   → `resolved`; `Err` → `fail_task_step` with the message (owner dropped
   the task since creation). `MAX_TASK_DEPTH` (`dispatch.rs:136-137`) is
   checked before this, as today.
3. **Step input** renders in the **caller's** context (`Scope::ChildTaskInput`,
   `dispatch.rs:158-163`). Unchanged. The result is the **caller bucket**
   `C`; its keys are what the caller supplied.
4. **Action defaults** come from the **persisted** `action_spec.input`
   (serialized at creation, `job_creator.rs:542`), not from a live lookup of
   `step.action_name` in any actions map — this removes the caller/owner
   name collision of § 1 and the "wrapper edited after creation" drift.
   They are merged with `merge_action_defaults(&C, &schema, &{secret:
   O_cfg.secrets})` (`template.rs:791-825`): only fields absent from `C` are
   filled and rendered (`:811-816`), with `O`'s **live** secrets. What is
   pinned is the **template source**: a persisted `default: "{{
   secret.TOKEN }}"` still evaluates against dispatch-time `O_cfg.secrets`
   (workspace loading renders `secrets:` and connections, not action
   defaults, `workspace_loader.rs:169-175`, `models/workflow.rs:1024-1045`),
   so rotating or removing `TOKEN` changes the value or fails the step; a
   literal default stays the creation-time literal. A string default is
   rendered once in `merge_defaults` (`:520-527`) and again by
   `render_value_deep` (`:816`, `:736-739`), so a default whose first
   result contains `{{` is interpreted twice — existing helper behaviour,
   documented and tested (§ 6). The keys `merge_action_defaults` adds are
   the **action-default bucket** `D`. No connection resolution happens
   against the action schema any more (today `dispatch.rs:210`; § 7).
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

   For each connection-typed field of `task_schema` (primitives skipped,
   `template.rs:590-591`) present in a bucket:

   - **Boundary rule, per bucket.** If the bucket's value workspace
     differs from `T` (`A ≠ T` for `C`, `O ≠ T` for `D` — each judged on
     its own, so `C` can be local while `D` is foreign and vice versa) the
     value **must be a string** (a connection name); anything else —
     object, array, number, bool, null — is an error: `input '{field}': a
     connection passed across workspaces must be a connection name, got
     {type}`. Objects reach this point through a literal object in the
     flow step (`template.rs:457-458`), a template rendering to an object
     — including a prior step's output (`:433-436`) — or an object-valued
     default (`:532`); `resolve_connection_inputs_scoped` passes every
     **object** through unchecked (`:600-602`) and already rejects arrays
     and scalars (`:603-606`), so across a boundary objects are refused
     rather than trusted. For a local bucket the existing behaviour stays:
     objects pass through, other non-strings fail — same-workspace
     behaviour is unchanged and the local object-trust limitation is
     inherited (TODO.md). The rule runs before any resolution and uses the
     same primitive classification as the resolver (`:590-591`).
   - **Names** resolve with `resolve_connection_inputs_scoped`
     (`template.rs:580`) using the **task's** schema: `C` with
     `ResolveScope { schema_ws: T, value_ws: A, fallback_ws: Some(T) if A ≠
     T }`; `D` with `{ schema_ws: T, value_ws: O, fallback_ws: Some(T) if O
     ≠ T }`. A bare name resolves first in the workspace whose YAML wrote
     it, then in `T` only if the connection is `shared: true`
     (`resolve_connection_ref`, `:247-320`); a qualified `ws.conn` goes
     through the ordinary gate. The connection's declared type, canonical
     in its own workspace, must equal the field's (`:611-633`); a **named
     untyped connection** is accepted for any field type — the existing
     compatibility rule (`:620-621`), unchanged.

   Fields the task's own defaults will fill are absent from both buckets
   and are left to the creator (step 6). `lookup` is
   `WorkspaceSet::load(workspaces, T, Some(&resolved.config))`
   (`workspace_set.rs:26-34`): every loaded config is in the set and
   `local_override` pins `T` to the snapshot step 2 resolved against
   (`:78-83`). An error → `fail_task_step`.
6. **Create** with the owner's snapshot:
   `create_job_for_task_inner(workspaces, pool, &resolved.config, &T,
   &resolved.task_name, input, "task", Some(&source_id), Some(job_id),
   Some(&step.step_name), revision, CreationMode::Normal, None, defaults)`.
   Inside, `merge_defaults` fills the task's own defaults with `T`'s secrets
   (`job_creator.rs:281-283`) and `resolve_connection_inputs` resolves them
   ungated in `T` (`:315-317`) — the owner reading its own config; objects
   produced by step 5 pass through (`template.rs:600-602`). `raw_input` is
   the input as passed (`:278`). The dispatch bookkeeping around this call
   (`update_input`, `mark_running_server`, `mark_running_if_pending_server`,
   `dispatch.rs:239-250`) is unchanged — its known weaknesses are § 4.
7. **Revision.** `revision = if T == job.workspace { job.revision } else {
   workspaces.get_revision(&T) }` (`workspace/mod.rs:473`). Same-workspace
   children keep inheriting the parent's revision; a foreign child gets the
   owner's revision at the moment it is created. **Consistency contract:**
   the config snapshot (`resolved.config`, taken once in step 2 and reused
   in steps 5–6) and the revision are separate reads; `do_reload` loads
   the source (revision advances) before swapping the config
   (`workspace/mod.rs:664-680`), so a reload between the two pairs a v1
   config with a v2 revision — steps built from v1 run against the v2
   tarball. A top-level execute has the same exposure (`tasks.rs:525` reads
   the revision separately from the config it holds); not widened here;
   closing it needs `WorkspaceManager` to hand out `(config, revision)`
   under one lock (TODO.md).
8. **Log.** The `tracing::info!` at `dispatch.rs:279` and the parent job's
   creation line gain ` in workspace '{T}'` when `T ≠ A`.

The child row is stamped `workspace = T`, `task_name = bare`,
`parent_job_id`, `parent_step_name`, `source_type = "task"` — the last three
as today (`dispatch.rs:266-269`).

**Error scrubbing.** `fail_task_step` scrubs with the caller config's
secrets only (`dispatch.rs:77-78`). A Tera error from step 4 (an `O`
default rendering `{{ secret.TOKEN | round }}`) or from the creator (a `T`
default) quotes the offending value. `handle_task_steps_pass` builds the
scrub list once per step as `collect_config_secret_values(A_cfg) ++ (O_cfg)
++ (T_cfg)` (`workspace_set.rs:103-109`; unfiltered — only empty strings are
skipped by the replacer, `:125`), `T_cfg` once step 2 has it, and
`fail_task_step` takes the list instead of a config. `redact_secrets_in_str`
replaces values one after another against already-modified text
(`:122-130`), which is incomplete whenever occurrences intersect: a caller
secret that is a prefix of an owner secret (`prefix` vs
`prefix-sensitive-token`) leaves the suffix legible; two values whose
occurrences *cross* — `incorrect value: got "ABCD` (a caller value shaped
like Tera's own error text, `:117-118`) and `ABCD-token` — leave
`-token"` when the longer is masked first (and a different fragment when
the owner value is masked first); and a
self-overlapping value (`aba` in `ababa`) is never fully covered by
non-overlapping replacement. The helper is therefore redefined as
**span-union masking**: find every occurrence of every value in the
*original* text (overlapping occurrences included), merge intersecting or
adjacent spans, and replace each merged span with `••••••`. A masked span
is never re-scanned. This is a change to the shared helper, strictly
safer for every caller; unit-tested with containment, crossing, equal-
length and self-overlap cases (§ 6). Scrubbing at the write covers the job log,
`job_step.error_message`, `retry_history`, and every reader including MCP
`get_job_status`, which returns the persisted text without the REST
redactor (`mcp/tools.rs:570-580`). A secret rotated out of the loaded
config is not scrubbed — inherited.

### 3.4 What does not change — verified

Consumers downstream of creation key off the **child row's own**
`workspace` / `task_name`:

- **Propagation** — `Settlement::propagate` writes the parent step from
  `child_job.status` / `.output` and calls `advance(parent_job_id)`
  (`propagate.rs:135-150`); `advance` re-reads the parent row and resolves
  *its* workspace (`settlement/mod.rs:201`, `:135-136`).
- **Terminal handling, hooks** — resolved from the terminal job's own row
  (`settlement/mod.rs:135-169`). A child is not a top-level source
  (`hooks.rs:78-83`), so only `T`'s *task-level* `on_*` hooks fire, in `T`
  (`hooks.rs:284-293`).
- **ACL** — no per-child check exists today: `execute_task` checks `Run` on
  the top-level task only (`web/api/tasks.rs:435-477`) and dispatch never
  consults ACL. Unchanged (decision, revision 1). Job detail / logs /
  artifacts check `View` against the row's `(workspace, task_name)`
  (`web/api/jobs.rs:1141-1161`), so the child is governed by `T`'s rules;
  on Deny the detail endpoint answers **404** (`jobs.rs:304-305`), and
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
pre-check catches that direct form (§ 3.2). A cross-workspace cycle
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
  actions binds in `T`'s own scope with no `shared` gate — the worker
  prepares a local action in the local scope (`rendering.rs:134`,
  `template.rs:258-265`) — exactly as when `T`'s own trigger runs it.
  Undeclared extra fields pass through `merge_defaults` untouched
  (`template.rs:509`); required fields are not enforced at creation
  (`:536-538`).
- What crosses back to `A`: the child's terminal status and its **output,
  verbatim** (`propagate.rs:135-141`) — deliberate disclosure, the caller
  asked for the result. A task that emits credentials in its output
  discloses them to every caller.
- What never crosses: `T`'s secrets and config are not rendered in `A`'s
  context; the child's detail is `T`-ACL'd and redacted; render errors are
  scrubbed with all three configs' secrets by span union (§ 3.3).
- A child is **not** the owner's scheduler run: it takes no task-level
  retry (`terminal::plan` gates on `parent_job_id.is_none()`,
  `terminal.rs:185-187`), fires no workspace-level hooks, and is cancelled
  by the caller's cancellation (subject to the enumeration race in § 4).
  Established child semantics, documented.

### 3.7 Parent → child link

A child's detail already carries `parent_job_id` (`web/api/jobs.rs:274`)
and `source_id = "{parent}/{step}"` (`dispatch.rs:252`, rendered as text at
`job-detail.tsx:348-350`); the **parent's** step DTO has no pointer to its
child, and a cross-team child is in `T`'s job list only. The step DTO gains
`child_jobs: Vec<{ id, workspace, task_name, status, created_at }>` for
`type: task` steps, newest first, from the new
`JobRepo::get_child_jobs_for_step(parent, step)` (`WHERE parent_job_id = $1
AND parent_step_name = $2 ORDER BY created_at DESC, job_id DESC`). This is
**execution history**, not attempt identity: a step retry reuses the
parent step row (`job_step.rs:769-779`) and creates another child — but
resets the row *before* that child exists — and duplicate dispatch (§ 4)
can create two children for one attempt, so the list makes no claim about
which entry is "current". The UI labels it "child jobs" and links each to
`/jobs/{id}`; the link follows the child's own ACL (404 if denied).
Attempt association is the lifecycle spec's.

## 4. Carried lifecycle risks

These exist today for every `type: task` child; the feature reuses the
same dispatch, propagation and settlement mechanisms unchanged, so a
cross-workspace child has exactly the same exposure — plus the operational
one that the two workspaces are now managed by different teams. They are
owned by `2026-09-16-task-step-lifecycle-hardening-design.md`, which takes
revision 2's review as its problem statement. A cross-team caller should
know them, and § 9's guide states each:

- **Dispatch is not exclusive.** Two `advance` calls on the same job can
  both read a ready `type: task` step (`dispatch.rs:107-122`);
  `mark_running_server` discards `rows_affected` (`job_step.rs:630-645`),
  so both create a child — the owner's task runs twice. A dispatcher
  working from a stale read that then hits a render or default-merge
  error fails the step through the unguarded `fail_task_step`
  (`dispatch.rs:81`) even though another dispatcher succeeded.
- **Dispatch is not crash-safe.** The step is marked `running`
  (`dispatch.rs:247`) before the child's transaction commits
  (`job_creator.rs:460`); a crash in between leaves a `running` step with
  no child and no worker. Nothing repairs it automatically: a step
  timeout, if configured, fails or *retries* it (`job_step.rs:1083-1093`,
  `recovery.rs:140`), a job timeout cancels the job, an operator can
  cancel it. A child committed before its `init` ran has no automatic
  initialisation recovery if its root steps are server-managed
  (`job_step.rs:570`, `:1116`).
- **Terminal delivery is not atomic.** A child's one-shot terminal claim
  (`terminal.rs:43-45`, taken at `settlement/mod.rs:270`) precedes the
  parent-step write (`:282`); a crash between them consumes the claim
  without delivering the result to the caller's step.
- **Cancellation can miss a child** committed after `cancel`'s enumeration
  snapshot (`job.rs:771-778`, `settlement/mod.rs:748`) by a dispatcher
  working from an earlier readiness read.
- **A timed-out parent step does not cancel its child**, and the child's
  later completion overwrites the step with the unguarded `mark_completed`
  / `mark_failed` (`recovery.rs:117-159`, `propagate.rs:135-148`,
  `job_step.rs:665-710`); with a step retry the stale child can settle the
  *replacement* attempt (`job_step.rs:754-789`).
- **A job whose task is removed mid-run is not settled by its own
  steps**: `resolve` returns `None` (`settlement/mod.rs:160-167`), `advance`
  returns early for a non-terminal job (`:209-211`), no sweep re-enters
  it. Absent a job timeout (which cancels it without needing the task
  definition, `job.rs:884-889`, `recovery.rs:205-214`) or an operator
  cancel, it stays `running` and the caller's step waits. Independent
  workspace reloads make this a cross-team contract: **removing a task
  that other workspaces call strands their running jobs** — the guide
  says so.
- **A child suspended at creation fires no `on_suspended` hook**:
  `dispatch::init` suspends via the pool-tier `handle_approval_steps`
  (`dispatch.rs:315`, `:452`) and no child path reaches
  `fire_initial_suspended_hooks` (`:489-565`; callers are the six top-level
  entry points only). A `B/deploy` that opens with an approval waits
  silently when called from `A`.
- Config and revision read separately (§ 3.3 step 7); a job under an
  unavailable workspace stalls until its next step event; same-workspace
  object values in connection-typed fields are trusted (§ 3.3 step 5).

## 5. Validation (`stroem-common`)

`validate_workflow_config_inner`'s `type: task` check (`validation.rs:65-78`):

- CLI path (`libraries_resolved == false`, `local/validate.rs:21`; errors
  fail the run, `:33-35`): a dotted `task:` that is not a local key is
  skipped **with a warning** (`cannot validate cross-workspace task
  reference '…' offline`) — today it is skipped silently as a presumed
  library name (`:69-70`). A dotted name with an empty side (`.deploy`,
  `B.`) is an error in both modes.
- Server path: `CrossWorkspaceActionResolver` (`validation.rs:19`, `&dyn
  Fn(&str, &str) -> bool`) becomes a trait `CrossWorkspaceResolver { fn
  has_action(&self, ws, name) -> bool; fn has_task(&self, ws, name) ->
  bool }`; the existing closure call sites become `has_action`. A dotted
  `task:` that misses locally is accepted iff `has_task(ws, name)`; with no
  resolver (`validate_workflow_config_with_libraries`) it is an error, as
  today. The resolver validator is still not wired into any server load
  path (pre-existing, CLAUDE.md); § 3.2 is the enforcement.
- **Hooks.** Hook actions are not resolved cross-workspace (§ 8). A
  `type: task` action whose `task:` is not a local key, referenced from any
  `on_*` hook (`validate_hook_action_exists`, `validation.rs:1824`, call
  sites `:339,347,355,363,521,529,537,545`): **server path** → error `hook
  '{label}' uses action '{a}' whose task '{ref}' is in another workspace;
  hook actions cannot call tasks across workspaces`; **CLI path** → the
  offline warning above (it may be an unresolved library task — the CLI
  cannot tell, and must not fail a valid config, `:69-70`). The runtime
  path (`hooks.rs:566-579`, which hands the raw name to the local creator)
  bails with the server wording, so an unvalidated config fails the hook
  job clearly instead of with `not found`.
- `check_task_self_reference` stays bare-name; the qualified direct form is
  caught at creation (§ 3.2).

## 6. Tests

Each pins one seam. The multi-workspace fixture
(`setup_multi_workspace_with`, `integration_test.rs:8973-9095`) has both
workspaces at `"test-rev"` with `auth: None`, `acl: None`, an `InMemSource`
that reloads to the same config, and returns only router/pool/dir/container
(`:8975-8979`; the manager moves into state at `:9091`). It gains an options
struct (`MultiWsOpts { revisions, acl: Option<AclConfig>, auth: bool }`),
an `Arc<Mutex<WorkspaceConfig>>`-backed source per workspace so a test can
change one and `mgr.reload(name)`, a handle to the manager in its return,
and a third workspace `C`.

- **`stroem-common` unit** — `resolve_task_input_by_provenance`: caller
  value found in `A`; caller bare name absent in `A`, present-and-shared in
  `T`; present-but-unshared → `is not shared`; action default naming `O`'s
  own private connection resolves ungated; action default naming `T`'s →
  shared gate; declared-type mismatch → error; named untyped connection
  accepted; **boundary rule**: an object, array, number, bool or null in a
  connection-typed field is refused when the bucket is foreign — `A ≠ T`
  for the caller bucket, `O ≠ T` for the default bucket, each tested with
  the other bucket local (`A == T ≠ O` and `O == T ≠ A`); for a local
  bucket an object passes through and an array / scalar fails as today
  (`template.rs:600-606`); `A == O == T` equals the local result.
  `redact_secrets_in_str`: containment (`prefix`, `prefix-sensitive-token`),
  crossing (`incorrect value: got "ABCD`, `ABCD-token` in `incorrect
  value: got "ABCD-token"`), equal-length overlap, and self-overlap (`aba`
  in `ababa`) — every byte of every occurrence masked, in any list order;
  a value containing `•` and a value equal to a substring of `••••••` are
  matched in the original text only (the mask is never rescanned); empty
  values are ignored; a multi-byte value adjacent to another keeps valid
  UTF-8 boundaries; a non-overlapping matcher (`str::matches`-style) fails
  the self-overlap case, pinning that overlapping search is required.
  Validation: dotted `task:` CLI skip + warning; server accept / reject via
  `has_task`; local flattened key wins over the split; empty side rejected;
  hook with a qualified task → server error, CLI warning; an unresolved
  library task wrapped by a local hook action → CLI warning, not error.
- **`job_creator` unit** — `resolve_task_ref`: local hit, library-flattened
  hit, qualified hit, unknown workspace, unavailable workspace,
  loaded-but-no-task; each error's exact phrase. Pre-check: direct
  self-reference `A.p` inside `A` → the `(invalid)` message; literal caller
  connection name checked against the task schema, not the wrapper's
  action schema; literal object / null on a foreign call → error wrapped
  in `Failed to resolve connection inputs`; the same on a `when`-guarded
  step → no error at creation. `classify_execute_error`: `"has no task"`
  → 400; the wrapped boundary error → 400; an unavailable owner inside
  the wrapped chain → 500.
- **Integration, resolution** — Form A creates a child with `workspace =
  B`, `task_name = "deploy"`, `revision = B`'s (≠ `A`'s); completing it
  propagates output to the parent step and the parent completes. Form B
  does the same; its defaults come from the persisted `action_spec` even
  after `B`'s `run-deploy` is edited and reloaded, while a `{{ secret.* }}`
  default follows a rotated secret. Form B whose owner's `task:` names `C`
  (`A ≠ O ≠ T`) resolves relative to `B`. A persisted library action
  (`common.run-deploy` with `task: common.deploy`) stays a whole-key local
  lookup. **Name collision regression:** `A` has its own `deploy`; `action:
  B.run-deploy` now runs `B/deploy`, and the parent's `[task]` log line
  names `B`. Same-workspace child still inherits the parent's revision.
  Two-pass default: `default: "{{ secret.X }}"` with `X = "{{ secret.Y }}"`
  yields `Y`'s value (pins existing behaviour).
- **Integration, provenance through HTTP** — wrapper in `B` declaring a
  Redis connection input forwarding into a task expecting PostgreSQL: the
  submit is accepted (task-schema pre-check) and the child gets the
  PostgreSQL connection; a literal object in a connection-typed field on a
  cross-workspace call → 400 at submit; the same via `{{ }}` → step fails
  at dispatch with the boundary message; a `when`-guarded step with a
  literal object skips the pre-check (201 at submit) and, with a **true**
  guard, fails at dispatch — with a false guard it is skipped and never
  fails.
- **Integration, errors** — execute: `task: nope.deploy` → 400, `task:
  B.nope` → 400, `B` a placeholder → 500, `task: A.p` inside `p` → 400;
  `A → B → missing C` → 200 and `A`'s step fails with `B`'s message. Owner
  task removed **before** dispatch → step fails, dependents skip, job
  fails. A default in `B` rendering `{{ secret.TOKEN | round }}` with a
  non-numeric token, where `A` has a secret that is a prefix of `TOKEN`:
  the persisted step error, the job log, and MCP `get_job_status` all show
  `••••••` and no suffix.
- **Integration, access** — with `MultiWsOpts { acl: Deny-by-default +
  Run on A/pipeline }`: 200 on the parent, **404** on the child detail and
  logs; admin sees both; the parent's step DTO carries `child_jobs` and the
  link target 404s for the restricted user. After a step retry the list
  has two entries, newest first, and the DTO makes no "current" claim.
- **Integration, hooks** — `B/deploy`'s `on_error` fires in `B`; `A`'s
  workspace-level `on_error` does not fire for the child; `B`'s task-level
  `retry` does not create a retry job for the child; a qualified task in a
  hook action fails the hook job with the § 5 wording.
- **`tests/e2e.sh`** — one cross-workspace `type: task` step, server ↔
  worker: `B`'s step reads a file that exists only in `B`'s tarball and a
  `{{ secret.* }}` that exists only in `B`, and its output round-trips to
  `A`'s parent step.

## 7. Behaviour corrections (release notes)

1. `action: B.run-deploy` (a cross-workspace `type: task` action) now runs
   `B`'s task. Before, it ran the **caller's** task of the same bare name if
   one existed, else failed.
2. A `type: task` action's own `input` defaults are taken from the action
   definition persisted on the step at job creation, not re-read at
   dispatch: editing the action no longer changes an in-flight job's child.
   Secrets the defaults reference are still read live at dispatch, and the
   referenced task definition is still resolved live.
3. Connection-typed fields on a `type: task` **action's** schema are no
   longer resolved against the action schema — at creation (literal
   pre-check) or at dispatch. Resolution happens once, against the
   **task's** schema. A field the action declares as a connection but the
   task declares as a primitive now arrives as the name string, not the
   resolved object.
4. On a cross-workspace call, a non-string value in a connection-typed
   task input is rejected. It is a 400 at submit only when the value is a
   literal supplied by the caller's flow step, the step has no `when`, and
   the caller is not the task's workspace; a templated value, a value on a
   `when`-guarded step, or a foreign action default fails the step at
   dispatch if the step is reached. Within one workspace an object is
   still accepted as before.
5. A `type: task` action referencing a task by qualified name is now
   pre-checked at submit (400 on unknown workspace / no such task /
   self-reference); a hook action wrapping such a task is rejected by
   server-side validation and fails the hook job with a clear message.
6. The job detail step DTO carries `child_jobs` for `type: task` steps.

## 8. Out of scope

The lifecycle gaps of § 4 (owned by
`2026-09-16-task-step-lifecycle-hardening-design.md`); cross-workspace hook
actions and cross-workspace `agent` steps rendering against the owner (both
still deferred); a `shared:` flag on tasks (decided against); per-child ACL
(decided against); pinning the owner revision at parent creation (rejected,
§ 2); ancestry-based cycle detection (§ 3.5); atomic `(config, revision)`
reads; same-workspace object trust in connection-typed fields;
event-source backpressure counting across workspaces.

## 9. Documentation

- `docs/src/content/docs/guides/cross-workspace-references.md`: new section
  "Calling tasks in other workspaces" — both forms, the `A / O / T` rule and
  precedence, "input renders in the caller, defaults come from the action's
  owner, the child runs as the task's owner", the connection provenance
  and boundary rules, the revision rule and how it differs from actions,
  the trust model (§ 3.6) in user terms, ACL and visibility (404), hooks /
  retry / cancel semantics of a child, the depth bound for cycles, the
  parent → child link, and every § 4 caveat a cross-team caller must
  know — double dispatch, a stale dispatcher failing a dispatched step,
  a child missed by cancellation, a child running on after its parent
  step timed out, stranding on task removal, the terminal-delivery
  window, silent approvals; remove
  the first "Not yet supported" bullet (`:245`) and state that hook actions
  still cannot.
- `docs/src/content/docs/guides/action-types.md` `type: task` section: § 7.
- `CLAUDE.md` § Cross-Workspace References: replace the deferred bullet with
  the `A / O / T` rule, `resolve_task_ref`, the boundary rule and the
  span-union scrub; § Task Actions: the persisted-`action_spec`
  defaults and the task-schema pre-check. `CONTEXT.md`: "Owner workspace
  (action owner / task owner)".
- `docs/internal/TODO.md`: the § 4 risks with a pointer to the lifecycle
  spec; "ancestry-based cycle detection"; "atomic (config, revision)";
  "same-workspace object trust in connection inputs".
