# Git Refs on Action, Task and Trigger References — Design

Status: revision 1, draft — pending Codex review
Ships in: next minor (migration `049`)

Lets a flow step's `action:`, a `type: task` action's `task:` and a
trigger's `task:` name a git **ref** (branch, tag or full commit SHA) of
their owner workspace, so several releases of one workspace can run side by
side, each from its own definitions and files. Line numbers cite
`anatolii/Revisions` at `b367b6c`.

## Revision history

**Revision 1 (2026-10-02).** First draft, from the brainstorming session.

## 1. Goal

**Primary.** Several release branches (or tags) of one workspace run at the
same time. The default branch's YAML (`main`) is the manifest: it pins which
release each step, task or trigger runs. Cutting a release is a reviewed PR
to `main` that bumps a `ref:`.

**Secondary, deferred.** Testing a feature branch before merge (a per-run
ref override) is out of scope (§ 16).

**Success criteria.**

1. Task `nightly` from `release/2.3` and from `release/2.4` run concurrently,
   each with its own YAML (flow, inputs, actions, secrets, connections) and
   its own files.
2. A job started on a ref runs **one commit** for its whole life, even if the
   branch moves mid-job.
3. Cutting `release/2.5` needs no server configuration change.
4. Jobs that use no `ref:` behave exactly as today (one exception, § 5.4).

## 2. Background: what a revision means today

- A git workspace tracks one branch (`WorkspaceSourceDef::Git.git_ref`,
  `config.rs:257-280`) in ONE working directory
  (`std::env::temp_dir()/stroem/git/{ws}`, `workspace/git.rs:28-31`), and the
  manager holds ONE published config per workspace
  (`WorkspaceEntry.published`, `workspace/entry.rs:46`). `peek_revision`
  matches only `refs/heads/{git_ref}` (`git.rs:343`); a tag or SHA cannot even
  be the configured ref (`guides/multi-workspace.md:67`).
- `job.revision` pins **files only, and only partly**. The worker fetches
  `?revision=` (`stroem-worker/src/poller.rs:252-258`); the server serves it
  from the replica-local tarball cache or, if the requested revision is not
  the current one, answers 404 "Revision '…' no longer available"
  (`web/worker_api/workspace.rs:129-135`). Nothing rebuilds an old revision
  from git.
- **Definitions are read live** during a job's life: the task flow on every
  `Settlement::advance` (`settlement/mod.rs:135`), step `input:` and action
  defaults at claim (`web/worker_api/rendering.rs:38-134`), secrets,
  connections, hooks, `type: task` children and task retry.
- Cross-workspace references already run an action "in the owner's context":
  `job_step.action_workspace` / `action_revision` (migration 043) are stamped
  at creation, `claim_job` renders against the owner and points the worker at
  the owner's tarball (`web/worker_api/jobs.rs:876-893`), and a
  cross-workspace `type: task` creates its child in the task owner's
  workspace (`settlement/dispatch.rs:138`, `job_creator.rs:715`). This design
  extends that model with a commit.
- Workflow models do not use `deny_unknown_fields`
  (`stroem-common/src/models/workflow.rs`): an unknown YAML key is silently
  dropped. That shapes § 4.6 and § 13.

## 3. Decisions

| # | Decision | Chosen | Rejected alternatives |
|---|---|---|---|
| D1 | Who picks the ref | Static `ref:` in YAML, next to the name it qualifies | Per-run override (deferred), templated refs |
| D2 | Where | Flow-step `action`, `type: task` action, scheduler + webhook trigger `task` | Hooks, event sources, agent task tools (rejected loudly, § 4.6) |
| D3 | Ref forms | Branch, tag, full 40-hex SHA | Short SHAs |
| D4 | Context of a ref'd target | The owner **at that commit**: definitions, files, secrets, connections | `main`'s secrets; merged secrets |
| D5 | Pinning | A job on a ref runs one commit for life; every ref in a job is resolved when that job is created | Follow the branch mid-job; pin every job (too broad) |
| D6 | Task / global state | Isolated per ref string (`task_state.ref`, `workspace_state.ref`) | Shared; opt-in key. No migration of existing rows (state is unused today) |
| D7 | Trust gate | **None** — any config may reference any ref of any git workspace (accepted risk, § 12) | Allow-lists in server config or owner YAML |
| D8 | Freshness | Lazy: a branch is re-checked only when a reference to it is resolved, at most every `poll_interval_secs` | Warm-up on publish; background tracker (both follow-ups) |
| D9 | Architecture | Separate `PinStore` keyed by `(workspace, commit)` with immutable checkouts | One synthetic workspace entry per ref; snapshotting config into the job row (would persist rendered secret values) |

## 4. YAML surface and resolution

### 4.1 Where `ref` goes

The field sits next to the name it qualifies. Rust field `git_ref`,
`#[serde(default, rename = "ref")]`, `Option<String>`.

```yaml
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
    task: nightly             # own task @ commit
    ref: 3f2a9c0e…            # full 40-hex SHA

triggers:
  nightly-billing:
    type: scheduler
    cron: "0 2 * * *"
    task: billing.nightly     # NEW: cross-workspace trigger task
    ref: v4.1.0
```

- `FlowStep.git_ref` qualifies `FlowStep.action`.
- `ActionDef.git_ref` qualifies `ActionDef.task`; only valid with
  `type: task` (validation error on any other action type).
- `TriggerDef::Scheduler.git_ref` and `TriggerDef::Webhook.git_ref` qualify
  the trigger's `task`.

### 4.2 Ref forms

| Written | Meaning |
|---|---|
| 40 hex chars | Commit SHA (normalised to lowercase) |
| `refs/heads/<name>` | Branch `<name>` |
| `refs/tags/<name>` | Tag `<name>` (peeled to its commit) |
| any other `<name>` | Branch `<name>` if it exists, else tag `<name>` |

A bare name must be a valid git refname (`git2::Reference::is_valid_name`
on `refs/heads/<name>`). A bare name that is not found and is 4–39 hex chars
gets the hint "short commit SHAs are not supported; use the full 40-character
SHA". A ref containing `{{` or `{%` is a validation error — refs are never
templated (D1).

### 4.3 The resolution rule

Every reference is written inside some config `C`. `C` is either the live
config of workspace `W`, or the **pinned** config of `W` at commit `X` with
ref string `R0` (a pinned job's config, or an owner config reached through a
ref). The *base workspace* of a reference is `W`.

**Without `ref:`** — exactly today's resolution (library item → local name →
`ws.item` on a local miss; `type: task` via `resolve_task_ref`,
`job_creator.rs:715`), with one addition: if `C` is pinned and the resolved
owner is `W` itself, the reference **inherits `C`'s pin** `(W, X, R0)`. A
qualified `ws.name` that leaves `W` stays live (today's cross-workspace rule).

**With `ref: R`** — the owner is decided syntactically, then the name is
looked up in owner@R:

1. Name without `.` → owner `O = W`, local name = the whole name.
2. Name with `.`, prefix `p` before the first `.`:
   - `p` is a configured library name → 400 "library items have no ref"
     (libraries are server-level, flattened into every config).
   - `p` is a configured workspace → owner `O = p`, local name = the rest.
   - otherwise → 400 unknown workspace.
3. `O` must be a configured **git** workspace → otherwise 400. `O` need not
   be healthy: a pin does not depend on the owner's live entry (§ 5.1).
4. `pin = PinStore::resolve(O, R)`, `cfg = PinStore::ensure(O, pin.commit)`.
5. Look the local name up in `cfg` (`actions` for a flow step, `tasks` for a
   `type: task` action or a trigger). Missing → 400 using the existing
   phrases "has no action" / "has no task", plus "at ref 'R'".

The owner rule deliberately does not require the name to exist in `C`: a step
may call an action that exists only on the release branch.

| Case | Resolves to |
|---|---|
| Unqualified + `ref` | own workspace @ ref |
| `ws.name` + `ref` | `ws` @ ref |
| Unqualified, no `ref`, inside a pinned config | inherits the pin (same commit, same ref string) |
| `ws.name`, no `ref`, inside a pinned config | `ws` live (today) |
| Library item + `ref` | 400 |
| `ref` on a folder workspace | 400 |

### 4.4 When a ref becomes a commit

Once, when the job that **contains** the reference is created (D5):

- flow-step actions and `type: task` steps are stamped at parent-job creation
  (§ 6, § 7.1);
- a trigger resolves its ref at fire time (§ 7.5);
- a child job resolves its own nested refs at its own creation.

A branch moving mid-job therefore cannot split one job across two commits of
the same ref. Two different jobs may still see different commits (§ 5.2).

### 4.5 Ownership boundaries are keyed on the workspace name

The connection `shared: true` gate, the owner-side-render-error withholding
rule (CLAUDE.md § Secrets in logs; `settlement/dispatch.rs::handle_task_steps_pass`)
and ACL key on the **workspace name**, never on the ref. Own@`release/2.3` is
the same workspace as own@live; `billing@v4.1.0` is cross-workspace exactly
like `billing` today.

### 4.6 Unsupported places fail loudly

Because serde drops unknown keys, a `ref:` on a hook, an event-source trigger
or an agent `tools: [{task: …}]` entry would silently run the default branch.
`HookDef`, `TriggerDef::EventSource` and the agent task-tool entry therefore
also get the `git_ref` field, used only to fail:

- `stroem validate` and `validate_workflow_config*` report "`ref` is not
  supported on hooks / event sources / agent task tools yet".
- At runtime (validation is not wired into server load paths, CLAUDE.md §
  Cross-Workspace References): a hook with `ref` is not fired and the error is
  logged to the source job (same place as today's cross-workspace hook
  bail, `settlement/hooks.rs::fire_single_hook`); an event source with `ref`
  is not started and logs once per reconcile; an agent task-tool call naming
  a `ref`'d entry is rejected with 400.

## 5. PinStore

New module `crates/stroem-server/src/workspace/pins.rs`, one instance per
replica, owned by `WorkspaceManager`. It is fully separate from the watcher
path: the default-branch clone, the exec mutex, `Availability` and
`apply_load_result` are untouched.

### 5.1 Storage

- **Bare repo** per git workspace: `{pin_dir}/{ws}/repo.git`, cloned lazily
  on first use with the workspace's URL and auth (`GitAuthConfig`), using the
  same credential callback (`GitSource::build_remote_callbacks`, fail-fast
  rule) and the process-wide libgit2 timeouts. Never touches the watcher's
  working clone.
- **Checkout** per commit: `{pin_dir}/{ws}/trees/{sha}/`, written to a tmp dir
  and renamed into place — immutable once present, no `.git`. Uses
  `git2::build::CheckoutBuilder::target_dir`.
- **Config** per commit: in-memory `Arc<WorkspaceConfig>`, loaded from the
  checkout with the existing folder loader under a `LoadBudget`, libraries
  merged exactly as `apply_load_result` does (`workspace/entry.rs:162-164`).
- `pin_dir` defaults to `std::env::temp_dir()/stroem/pins` (same lifetime as
  today's clones: lost on container restart, rebuilt on demand).

A pin depends only on the owner being a configured git workspace (URL +
auth). If the owner's live load is failing (e.g. a YAML error on `main`), its
refs can still be pinned.

### 5.2 `resolve(ws, ref) -> Result<Pin>`

`Pin { workspace, ref_name /* as written */, commit /* 40-hex */ }`.

- Full SHA: returned as-is (existence is checked by `ensure`).
- Branch / tag: from one ls-remote (`connect_auth` + `list()`, as
  `GitSource::peek_revision` does) that lists every head and every tag
  (peeled `^{}` entries win for annotated tags). The listing is cached per
  workspace with a TTL of that workspace's `poll_interval_secs`
  (D8 — same freshness as the default branch).
- ls-remote **succeeds but the ref is absent** → `RefNotFound` (permanent).
- ls-remote **fails** (network, auth, timeout) → use the last cached listing
  if any, with a `warn!`; otherwise `PinUnavailable` (transient).
- Each replica has its own cache: two jobs created on different replicas
  within one TTL window may resolve a branch to different commits.

### 5.3 `ensure(ws, commit) -> Result<Arc<Pinned>>`

`Pinned { config: Arc<WorkspaceConfig>, dir: PathBuf, secret_values: Vec<String> }`.

1. Cache hit → return.
2. Commit object missing from the bare repo → fetch. For a ref resolved by
   name, fetch that ref (`refs/heads/R` / `refs/tags/R`), which every server
   supports. For a bare SHA, fetch the SHA; if the server refuses
   want-by-SHA, fall back to fetching all heads and tags, then look again.
   Still missing → `CommitNotFound` (permanent).
3. Checkout to the immutable tree dir (if absent).
4. Load the config (if not cached). A load error from the YAML itself is
   permanent (`PinLoadFailed`); a budget expiry or a `sops`/`vals` failure is
   transient (`PinUnavailable`).
5. Collect `secret_values` for redaction (§ 9).

`ensure_tree(ws, commit) -> Result<PathBuf>` runs steps 1–3 only (no config
load, no sops/vals). The tarball path (§ 5.4) uses it; everything that needs
definitions uses `ensure`.

Single-flight per `(ws, commit)`. Pin loads take a permit from a **separate**
semaphore (`MAX_CONCURRENT_PIN_LOADS`, 4), not the watchers'
`MAX_CONCURRENT_WORKSPACE_LOADS`, with a blocking acquire bounded by the load
budget — so creation-path pin loads never starve the watchers. All git and
YAML work runs on `spawn_blocking`.

Errors carry a typed kind (`PinError::{RefNotFound, CommitNotFound,
NotGit, PinLoadFailed, PinUnavailable}`) so callers classify by
`downcast_ref`, not by message text.

### 5.4 Tarballs

`download_workspace` (`web/worker_api/workspace.rs:44`), on a cache miss for
a requested revision that is not the current one **of a git workspace**,
calls `PinStore::ensure_tree(ws, revision)` and builds the tarball from that
checkout dir (plus the library overlay, as `build_tarball` does), caching it
under the unchanged key `(ws, revision)`. It 404s only when the commit does
not exist in the repo. Folder workspaces have no history and keep today's
404.

This is the one change to the default path: an **ordinary** job whose
revision fell out of the tarball cache is now served instead of 404'd. Its
tarball comes from a clean checkout and has no `.git` directory (today's live
tarballs include `.git` because `build_tarball` archives the working clone,
`workspace.rs:221-247`). The health gate stays: an errored workspace still
404s before the cache lookup (`workspace.rs:54-58`) — tracked as a follow-up
(§ 16), since the pin path itself would not need it.

## 6. Data model — migration `049_git_refs.sql`

All additive and nullable.

| Column | Set when | Meaning |
|---|---|---|
| `job.ref TEXT` | The job was created in owner@ref (ref'd `type: task` child, inherited child, ref'd trigger, hook/retry of a pinned job) | Ref string as written. `job.revision` holds the commit. `ref IS NOT NULL` ⇔ **pinned job** |
| `job_step.action_ref TEXT` | The step's action was resolved through a `ref:` | `action_workspace` (now also set when the owner is the job's own workspace) and `action_revision` (the commit) describe the pin |
| `job_step.task_workspace TEXT`, `task_ref TEXT`, `task_revision TEXT` | A `type: task` step whose task reference carries a `ref:`, or inherits a pin from a ref'd action | The task owner `T` and its pin, stamped at parent creation |
| `job_step.pin_releases INT NOT NULL DEFAULT 0` | A claim was released because its pin was unavailable (§ 7.2) | Bounds the release-to-ready loop |
| `task_state.ref TEXT`, `workspace_state.ref TEXT` | Snapshot written by a pinned job | Part of the key. `NULL` = unpinned (today's rows) |

A `type: task` step carries two owners: the action's owner `O`
(`action_*`) and the task's owner `T` (`task_*`). They are kept in separate
columns on purpose.

State lookups use `ref IS NOT DISTINCT FROM $n`. `idx_task_state_lookup`
(migration 028) and `idx_workspace_state_lookup` (029) are dropped and
recreated with `ref` inserted after the leading key columns. No existing row
is rewritten (D6).

## 7. Flows

### 7.1 Job creation (`job_creator`)

- A pinned job is created from its pinned config: `create_job_for_task_inner`
  receives `T`@commit, so its `build_step` calls resolve unqualified names
  inside that commit automatically (§ 4.3 inheritance).
- Flow step with `ref` → § 4.3 → `build_step(…, action_workspace = O,
  action_ref = R, action_revision = commit)`; `action_spec`, `action_type`,
  `runner`, `required_ability`, `required_tags`, retry come from the action
  at that commit. `precheck_literal_connection_inputs`
  (`job_creator.rs:757`) runs against `O`@commit.
- `type: task` action with `ref` → § 4.3 for the `task` name → stamp
  `task_workspace = T`, `task_ref`, `task_revision`.
  `precheck_task_step_literals` (`job_creator.rs:815`) checks the caller's
  literals against `T`@commit's task schema.
- `type: task` action **without** `ref` whose action came from a pin
  (`action_ref` set) and whose `task` is unqualified → stamp `task_*` with the
  action's pin (inheritance made explicit).

### 7.2 Claim (`claim_job`, `web/worker_api/jobs.rs:429`)

| Step / job | Config used for rendering and action resolution |
|---|---|
| `action_ref` set | `action_workspace`@`action_revision` |
| `action_workspace` NULL, job pinned | `job.workspace`@`job.revision` |
| otherwise | live, as today |

The caller's step `input:` renders in the job's own config (pinned if the job
is pinned), as today for cross-workspace steps. `ClaimResponse.workspace` /
`revision` point at the owner and the commit — the worker's file path is
unchanged. `ClaimResponse` gains `state_ref: Option<String>` = `job.ref`
(state belongs to the job's task, not to the step's owner).

**Transient pin failure at claim.** A claim can land on a replica that never
loaded the pin (HA) or that just restarted. `PinUnavailable` must not fail
the job:

- New primitive `JobStepRepo::release_claim(step, worker_id, retry_at)`:
  `running → ready` guarded by `status = 'running' AND worker_id = $w`,
  clears `worker_id` / `started_at`, sets `retry_at = now + 10 s`
  (`claim_ready_step` already honours `retry_at`). It is not a failure, does
  not touch `retry_attempt` or `retry_history`.
- The job log gets `[pin] {ws}@{ref} ({short sha}) not available yet on this
  server, retrying: {scrubbed error}` (`_server` step).
- The claim response is "no work".
- Backstops: the job timeout, when one is set; and a cap — `release_claim`
  increments `job_step.pin_releases`, and once it reaches
  `MAX_PIN_RELEASES` (30, ≈ 5 minutes at 10 s) the step fails through
  `fail_claimed_step` instead. Without the cap, a job with no timeout would
  loop forever on a permanently unreachable remote.

Permanent pin errors (`RefNotFound`, `CommitNotFound`, `PinLoadFailed`) go
through `fail_claimed_step` (`jobs.rs:364`) as any claim-time failure does.

### 7.3 Settlement

`Settlement::resolve(job)` (`settlement/mod.rs:135`) reads
`config_for(job.workspace, pin of job)`. Because a pinned config is
immutable, the cascade, hooks, retry and approvals of a pinned job all see
one commit.

| Derived job | Ref + commit |
|---|---|
| `type: task` child with `task_*` stamped | `task_ref` / `task_revision` |
| Unqualified `type: task` child of a pinned job (no `task_*`) | Parent's `ref` + `revision` |
| Hook job of a pinned job (`fire_single_hook`) | Source job's `ref` + `revision`; the hook definition comes from the pinned config |
| Task-level retry (`create_retry_job`, `settlement/retry.rs:108`) | Failed job's `ref` + `revision` |
| Re-run / Restart of a pinned top-level job | **Re-resolves** `job.ref` (today they take the current revision; for a ref that is its current commit) |
| Agent task-tool child of a pinned job (`agent_task_tool`, `jobs.rs:1073`) | Parent's `ref` + `revision`; the tool's task is looked up in the pinned config |

`dispatch::handle_task_steps_pass` picks the task's config: `task_*` if
set; else, if the action owner is the job's workspace and the job is pinned,
the job's pin; else live (today).

### 7.4 `config_for`

One function, `WorkspaceManager::config_for(ws, pin: Option<&PinRef>) ->
Result<ConfigHandle>`: `None` → today's live `get_config`; `Some` →
`PinStore::ensure`. Call sites: `Settlement::resolve`, `claim_job` (job and
step owner), `handle_task_steps_pass` / `resolve_task_ref`,
`agent_task_tool`, the state endpoints, `download_workspace`. Every other
consumer already receives its config from one of these.

### 7.5 Triggers

Scheduler (`scheduler.rs::fire_trigger`, `:346`) and webhook
(`web/hooks.rs`):

1. Resolve the task owner `T` (§ 4.3; without `ref`, `ws.task` is resolved
   like `resolve_task_ref` — local first, then cross-workspace).
2. With `ref`: resolve and ensure the pin.
3. The existing revalidation (the current defining config still defines the
   trigger identically) and availability checks.
4. **Only then** the concurrency policy, keyed — as today — on
   `source_id = "{defining_ws}/{trigger}"` (`count_active_by_source`,
   `get_active_job_ids_by_source`).
5. Create a top-level job in `T` with `job.ref` / `job.revision` (or `T`'s
   current revision when there is no `ref`). A `skip`-policy skipped row is
   recorded in `T` too.

Any failure in steps 1–3 logs `Trigger '…' MISSED: …` with no side effects —
no `cancel_previous`, no skipped row — the rule already used for an
unavailable workspace. `triggers: false` follows the **defining** workspace.
Webhook errors map as in § 8.

### 7.6 State

- Render context: `render_context::latest_snapshots` (`render_context.rs:50`)
  takes the job's ref and reads `TaskStateRepo::get_latest(ws, task, ref)` /
  the workspace-state equivalent.
- Worker: `poller.rs:371`/`:430`/`:622`/`:669` append `?ref=` when
  `state_ref` is set; the server's four state endpoints accept it.
- `POST /api/workspaces/{ws}/tasks/{task}/state` and `…/state` (manual
  upload) accept `?ref=`; the state list endpoints return `ref` per snapshot
  and accept a `?ref=` filter.
- Retention (`max_snapshots`) prunes per `(workspace, task, ref)`.
- Pre-existing, unchanged: the worker keys state on `ClaimResponse.workspace`
  (the step's owner) and the job's task name, so a cross-workspace step reads
  `(owner, caller task)`. Recorded in TODO.md, not fixed here.

### 7.7 Template and hook metadata

`render_context::job_context` (`render_context.rs:216`) adds `ref`
(`{{ job.ref }}`, `""` for unpinned jobs) next to `revision`; hooks get
`hook.ref`. `{{ job.revision }}` of a pinned job is its commit.

## 8. Errors and classification

`classify_execute_error` (`web/api/mod.rs:393`) gains a typed check (via
`downcast_ref::<PinError>()`, before the phrase tiers):

| Condition | Execute API | Trigger | At claim |
|---|---|---|---|
| Unknown workspace, folder owner, library item + `ref`, invalid ref syntax | 400 | MISSED | — (never created) |
| `RefNotFound`, `CommitNotFound` | 400 | MISSED | permanent → `fail_claimed_step` |
| Name missing at that commit ("has no action/task … at ref") | 400 | MISSED | permanent |
| `PinLoadFailed` (YAML at that commit does not load) | 400 | MISSED | permanent |
| `PinUnavailable` (ls-remote/fetch failure with no cached listing, budget, sops/vals) | 500 | MISSED | transient → `release_claim` (§ 7.2) |

For a `type: task` step, a pin error at dispatch fails the step through
`fail_task_step` (`settlement/dispatch.rs`), which already scrubs and
re-cascades; `PinUnavailable` there is also failed (dispatch has no
release-to-ready path) — but the pin was already ensured at parent creation
on some replica, so this needs a cold replica plus a git outage.

## 9. Secrets and redaction

- Withholding/scrubbing rules are unchanged and keyed on the workspace name
  (§ 4.5).
- **New obligation.** A commit's sops values, or how its vals references
  render, can differ from the live config. Every redaction set that today
  comes from loaded configs must also include `Pinned.secret_values` of every
  pin the job touches (`job.ref`, each step's `action_ref` / `task_ref`):
  - `workspace_set::collect_redaction_values` (`workspace_set.rs:227`) —
    the job-detail response (`input`, `raw_input`, step output);
  - the secret list passed to `redact_secrets_in_str` (`:163`) in
    `fail_claimed_step`, `fail_task_step` and the new `[pin]` log line
    (§ 7.2).
- Secret values in a pinned config are rendered once, at pin load, and stay
  until the pin is evicted (same as the live config, which re-renders only on
  a new revision).

## 10. Retention, limits, configuration

**Keep-set** (replica-local, evaluated on every replica's tick — not
leader-gated, because the store is local):

- `(job.workspace, job.revision)` of active jobs with `job.ref` set;
- `(action_workspace, action_revision)` of active jobs' steps with
  `action_ref`; `(task_workspace, task_revision)` with `task_ref`;
- failed pinned top-level jobs still owed a task retry (the 1-hour window of
  `tarball_keep_revisions`, `stroem-db/src/repos/job.rs:618`);
- plus the `keep_recent_per_workspace` most recently used pins.

Eviction drops the in-memory config and the checkout dir. The bare repo is
kept and never garbage-collected in v1. `tarball_keep_revisions` already
covers `job.revision` and `action_revision` of active jobs, so pinned
tarballs are retained by the existing tarball sweep.

**Config** (server, new optional section):

```yaml
pin_store:
  dir: /var/lib/stroem/pins        # default: <temp>/stroem/pins
  keep_recent_per_workspace: 5     # default 5
```

**Limits.** Pin loads use the `workspace_reload` budgets (`load_timeout_secs`
for fetch + checkout + load, `peek_timeout_secs` for ls-remote) and the
process-wide libgit2 timeouts. Separate semaphore of 4 (§ 5.3).

**Metrics** (`metrics.rs`, documented in `operations/metrics.md`):
`stroem_pin_loads_total{workspace,result}` and gauge
`stroem_pins_cached{workspace}`.

## 11. API, UI, CLI, worker

- `GET /api/jobs/{id}` returns `ref`; job steps return `action_ref`,
  `task_workspace`, `task_ref`, `task_revision`. Job lists return `ref`.
- UI: a `@ release/2.3 · 3f2a9c0` badge on Job Detail and on step rows that
  carry a pin; the commit links nowhere (no repo URL mapping in v1).
- CLI `stroem validate`: checks ref syntax and unsupported places; does not
  resolve refs (no server, possibly no network) — references with `ref:` are
  skipped with a warning, like dotted names today.
- Worker: `state_ref` handling (§ 7.6). No other change.

## 12. Security — accepted risk

There is no gate (D7). Anyone who can merge a `ref:` into **any** workspace
can make the server run **any** branch, tag or commit of **any** configured
git workspace with that workspace's secrets — including making the server
resolve whatever `vals` references an unreviewed branch writes. Mitigation
(follow-up): an allow-list per owner workspace, plus "a SHA must be reachable
from an allowed ref".

## 13. Rollout

- Migration 049 is additive; old code ignores the new columns.
- Server and workers ship together (`state_ref`). An old worker ignores the
  field and reads unpinned state — harmless today (state is unused) but
  documented.
- **Do not merge YAML that uses `ref:` until every server replica runs the
  release.** An old replica drops the unknown key and runs the default branch
  silently, and treats an own-workspace `action_workspace` as a live
  cross-workspace owner. Stated in the release notes and the guide.

## 14. Testing

**Unit (stroem-common).** Ref grammar (§ 4.2): SHA normalisation, `refs/…`
prefixes, invalid names, short-hex hint, templated ref rejected. Validation:
`ref` on a non-task action, on hooks / event sources / agent task tools.

**Unit (stroem-server, `file://` bare repos via `git2`, plain `#[test]` like
the GitSource tests).**
- The § 4.3 table as a pure resolver function over fake configs.
- `PinStore::resolve`: branch, tag (lightweight + annotated), SHA; a branch
  move is picked up after the TTL and not before; ls-remote failure falls
  back to the cached listing; deleted branch → `RefNotFound`; no cache +
  failure → `PinUnavailable`.
- `PinStore::ensure`: immutable checkout without `.git`; SHA fetch fallback;
  missing commit → `CommitNotFound`; YAML error → `PinLoadFailed`;
  single-flight (two concurrent ensures, one load); eviction honours the
  keep-set and `keep_recent_per_workspace`.

**Integration (testcontainers Postgres + `file://` repos).**
- A flow step with `ref` runs the release's action and files.
- `type: task` + `ref` creates the child in `T` with `job.ref`.
- **Regression: the branch moves mid-job; later steps and the child task
  still run the original commit.**
- Unqualified reference inside a pinned job inherits the pin; `ws.x` inside
  it stays live.
- Hook, task retry and agent task-tool children of a pinned job inherit
  ref + commit; re-run re-resolves.
- Cross-workspace trigger with and without `ref`; MISSED with no side effects
  on a missing ref (scheduler, via `fire_trigger_once`, `scheduler.rs:329`).
- Claim-time `PinUnavailable` → `release_claim` → step `ready` with
  `retry_at`; release cap → failed.
- `task_state.ref` isolation (two refs, same task) and NULL-safe lookup.
- Redaction covers a secret value that differs between `main` and the pin.
- Old ordinary revision tarball served instead of 404.
- `classify_execute_error` 400/500 for each § 8 row.
- `migration_test.rs`: 049 columns and indexes.

**E2E (`tests/e2e.sh`).** A git workspace backed by a local bare repo with a
`release/1` branch and a `v1.0.0` tag; a `main` task calling both; assert the
outputs differ by ref and that `job.ref` is set on the child.

## 15. Documentation

- New guide `docs/src/content/docs/guides/git-refs.md`: syntax, ref forms,
  resolution table, pinning, freshness (D8), state isolation, errors, the
  security note (§ 12), the rollout rule (§ 13).
- Updates: `guides/cross-workspace-references.md`, the triggers guide
  (cross-workspace `task` + `ref`), `guides/task-state.md` (per-ref
  isolation, `?ref=`), `guides/templating.md` (`job.ref`, `job.revision` of a
  pinned job), `reference/api.md` (new fields), `operations/metrics.md`,
  server config reference (`pin_store`). Regenerate `llms.txt`.
- CLAUDE.md: new § "Git refs (pinned references)"; CONTEXT.md: *Pin*,
  *Pinned job*, *PinStore*.
- `docs/internal/TODO.md`: the follow-ups in § 16.

## 16. Non-goals and follow-ups

- Per-run ref override (API/UI/MCP/CLI) — the feature-branch testing case.
- Allow-list / trust gate (§ 12).
- Warm-up on publish; background branch tracker (D8).
- `ref` on hooks, event sources, agent task tools (§ 4.6).
- Pinning definitions for **unpinned** jobs (today's live-config drift).
- Libraries at a ref; bare-repo `git gc`; sharing pins across replicas.
- Serving a cached or pinned tarball of an errored workspace (the
  `download_workspace` health gate).
- Pre-existing, found during design: the tarball-cache cleanup runs on the
  leader only (`recovery.rs:441`) while the cache is replica-local;
  cross-workspace steps key state on the owner workspace (§ 7.6).
