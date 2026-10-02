# Git Refs on Action, Task and Trigger References — Design

Status: revision 9 — Codex READY FOR PLAN at rev 7 (thread `01a0fb54`); revs 8–9 add plan/pre-flight amendments; owner-approved
Ships in: next minor (migrations `049` + `050`)

Lets a flow step's `action:`, a `type: task` action's `task:` and a
trigger's `task:` name a git **ref** (branch, tag or full commit SHA) of
their owner workspace, so several releases of one workspace can run side by
side, each from its own definitions and files. Line numbers cite
`anatolii/Revisions` at `b367b6c`.

## Revision history

**Revision 10 (2026-10-02, final review).** Fixes from the whole-branch review:
- I1: the re-advance phase also lists a `pending` pinned job that has a
  terminal step and no live one (§ 7.3).
- I2: a `sops`/`vals` failure of a commit is answered from memory for 30 s and
  becomes the permanent `PinLoadFailed` after an hour (§ 5.3, § 8).
- I3: pinned Re-run / Restart also require `Run` on the task's folder at the
  re-resolved commit (§ 7.3).
- M2: the redaction closure follows re-run sources too, under one cap with
  restarts, `MAX_SOURCE_LINEAGE_HOPS` (was `MAX_RESTART_LINEAGE_HOPS`) (§ 7.4).

**Revision 9 (2026-10-02, execution pre-flight).** Two rulings from the
pre-flight conflict scan:
- A permanent pin error during `advance` fails the job, so the re-advance
  phase cannot loop on it (§ 7.3).
- The webhook keeps its pre-refresh check against the cached secret, so an
  unauthenticated caller cannot trigger a git refresh. A failed refresh
  answers 500 (§ 7.5).

**Revision 8 (2026-10-02, implementation planning).** Three amendments found
while writing the plan:
- The `readvance_stalled_pinned_jobs` recovery phase (§ 7.3), so a pinned job
  whose `advance` hit `PinUnavailable` does not strand.
- Tarball 503 + `Retry-After`, with a bounded worker retry (§ 5.4).
- The `git_ref` column naming (§ 6).

Not yet reviewed by Codex; the implementation review covers them.

**Revision 7 (2026-10-02, Codex round 6, same thread).** The inventories
were incomplete. Added:
- task stats, which now exclude pinned jobs;
- webhook job-status polling, redacted in every branch, and the
  webhook-auth exception stated;
- whole-response redaction, which catches the copied `approval_message`;
- MCP artifacts in the table.

New § 7.9 makes the read-path inventory an implementation audit task, with
one test per path, rather than a list the design depends on being complete.

**Revision 6 (2026-10-02, Codex round 5, same thread).** I1–I5 CLOSED; log
redaction confirmed pre-existing. Two findings, applied by inventory rather
than path by path:
- J1: one helper (`job_task_path`, `check_job_acl(&JobRow)`) plus an
  exhaustive table of job-scoped read paths. This adds the WebSocket stream
  and worker detail, which derived the live folder inline.
- J2: worker detail's `error_message` joins the per-job redaction set (with
  row-level fail-closed), with the full list of `error_message` / output
  outlets.

**Revision 5 (2026-10-02, Codex round 4, same thread).** Explicit verdicts:
F6, G1–G12 CLOSED; event sources, keep-set vs eviction, retry window,
`hook_chain_depth` and MCP execute had no defects found.

Five findings, applied:
- I1: the role scope has three roles (caller / action owner / task owner),
  with a bucket → role table and a site → roles table.
- I2: MCP `get_job_status` uses the per-job redaction set and fails closed.
  Log-line redaction is pre-existing and goes to § 16.
- I3: the ACL test now matches § 7.8.
- I4: MCP `list_jobs` filters in SQL before `LIMIT`.
- I5: the migration is split into 049 (columns) and 050 (indexes under new
  names), so a `CONCURRENTLY` pre-run is possible.

**Revision 4 (2026-10-02, Codex round 3, same thread).** 4 findings, all
applied:
- H1: the connection lookup is role-scoped (caller / owner / others) at the
  pre-check, at claim and at dispatch, so an own-workspace ref at another
  commit cannot shadow the caller's commit.
- H2, H3: a pinned job's ACL folder is always its own `task_folder`. Lists
  authorise per job (pairs for unpinned, triples for pinned), in REST and
  MCP.
- H4: the migration's index locking is documented, with migration 009's
  `CONCURRENTLY` pre-build note.

**Revision 3 (2026-10-02, Codex round 2, same thread).** 12 findings; 11
applied, 1 cut from scope.

Applied:
- G1: `release_claim` locks the step, decides `Released` / `Cancelled` /
  `CapReached` / `NotApplied`, and cancels siblings itself when the job is
  terminal. The cap failure is guarded by the claim identity (G12).
- G2: state coordinates come from the job, not the worker's path. This
  fixes today's cross-workspace keying.
- G3: a git revision is served from cache or pin before the live health
  gate.
- G4, G5: the deferrals were refuted, because refs reach ref-only secrets.
  Owner-side claim render errors are now withheld (§ 7.2), and sync webhook
  output is redacted, failing closed (§ 7.5).
- G6: re-run and restart re-resolve a pinned source before task checks.
- G7: ACL folder rule and `job.task_folder` (§ 7.8).
- G8: a hook calling a ref'd `type: task` action is rejected.
- G9: skipped rows carry the resolved target.
- G10: a webhook is re-matched and re-authenticated after `force_refresh`.

Cut:
- G11: manual state upload stays unpartitioned in v1.

Still deferred as pre-existing: the sibling-claim-after-cancel race (§ 16).

**Revision 2 (2026-10-02, Codex round 1, thread `01a0fb54`).** 12 findings;
10 applied, 2 recorded as pre-existing and not worsened (§ 16):

- `release_claim` serialises with cancellation on the job row and settles a
  cancelled job's step as `cancelled` (F1).
- Recovery's running-step failures are guarded by the claim identity they
  observed, and release resets `ready_at` (F2).
- `ref` on an agent action is rejected in v1, because agent tools and MCP are
  built from the caller's config (F4, § 7.2).
- Post-creation init and initial `on_suspended` hooks derive their config from
  the created job (F6); webhook revalidation is pre-existing (§ 16).
- The inheritance rule for `type: task` is made explicit by stamping, and
  dispatch never infers it from the parent job (F7).
- The claim's prefix-strip invariant is stated and tested (F8).
- The state partition is derived from the job server-side; `state_ref` is
  dropped (F9).
- A per-job config set covers connection lookup, job detail and redaction,
  and fails closed (F10).
- `resolve` adopts the fetched tip, so a resolved commit always exists
  locally (F11).
- `pin_dir` is locked per process; eviction is lease-aware (F12).

Pre-existing and not worsened: F3 (claim-time owner render errors are
scrubbed, not withheld) and F5 (sync webhook output is unredacted).

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
`job_creator.rs:715`, whose base is the config the *action* is defined in),
with one addition: if `C` is pinned and the resolved owner is `W` itself, the
reference **inherits `C`'s pin** `(W, X, R0)`. A qualified `ws.name` that
leaves `W` stays live (today's cross-workspace rule). A pinned job that calls
a **live** foreign `type: task` action `B.run` whose `task:` is unqualified
resolves that task in `B` live: the base is `B`'s live config, not the job's
pin.

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

**Prefix-strip invariant (claim).** `claim_job` and
`prepare_step_action_input` strip a dotted `action_name` to its bare suffix
whenever `job_step.action_workspace` is set (`web/worker_api/jobs.rs:621`,
`rendering.rs:100`), because an owner stores its actions unqualified. Every
step this design stamps with `action_workspace` satisfies that: a ref'd name
is either undotted (rule 1, strip is a no-op) or `owner.x` (rule 2, strip
gives `x`). Library items never get a `ref` (rule 2, first bullet). A step
that **inherits** a job pin keeps `action_workspace = NULL`, so a
library-flattened name such as `common.pg-query` is looked up by its full key
in the pinned config, as today. Tested explicitly (§ 14).

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
- A hook whose `action` is a `type: task` action carrying `ref` is rejected
  the same way. Validation reports it, and at runtime `fire_single_hook`
  does not fire it and logs to the source job. Both hook creation branches
  read `ActionDef.task` directly and create the hook job without task
  resolution (`settlement/hooks.rs:594`, `:610`), so the `ref` would
  otherwise be dropped.

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
- **One process per `pin_dir`.** At startup the server takes an exclusive
  `File::try_lock` on `{pin_dir}/.lock` and refuses to start if another
  process holds it ("`pin_store.dir` is in use by another process"). A shared
  volume across replicas is therefore impossible by construction. Startup
  also removes stray `*.tmp-*` dirs left by an interrupted checkout. A bare
  repo that is not a usable repository is removed and re-cloned, the same
  rule `GitSource` applies to its clone dir (`workspace/git.rs:69-88`).

A pin depends only on the owner being a configured git workspace (URL +
auth). If the owner's live load is failing (e.g. a YAML error on `main`), its
refs can still be pinned.

### 5.2 `resolve(ws, ref) -> Result<Pin>`

`Pin { workspace, ref_name /* as written */, commit /* 40-hex */ }`.

- Full SHA: fetched by SHA if not local (fallback as in § 5.3 step 2);
  `CommitNotFound` if still missing.
- Branch / tag: from one ls-remote (`connect_auth` + `list()`, as
  `GitSource::peek_revision` does) that lists every head and every tag
  (peeled `^{}` entries win for annotated tags). The listing is cached per
  workspace with a TTL of that workspace's `poll_interval_secs`
  (D8 — same freshness as the default branch).
- ls-remote **succeeds but the ref is absent** → `RefNotFound` (permanent).
- ls-remote **fails** (network, auth, timeout) → use the last cached listing
  if any, with a `warn!`; otherwise `PinUnavailable` (transient).
- **The resolved commit always exists locally.** If the advertised OID is
  not in the bare repo, `resolve` fetches the ref **by name**
  (`refs/heads/R` / `refs/tags/R`) and adopts the fetched local tip as the
  resolution, refreshing the cached listing entry. A branch moved or
  force-pushed between ls-remote and fetch therefore yields the newer tip,
  never an OID that cannot be fetched. That is a legitimate resolution, since
  resolution happens before the job that records it exists (§ 4.4). Once a
  commit is stamped on a job or step, later `ensure` calls fetch that exact
  SHA (§ 5.3).
- Each replica has its own cache: two jobs created on different replicas
  within one TTL window may resolve a branch to different commits.

### 5.3 `ensure(ws, commit) -> Result<Arc<Pinned>>`

`Pinned { config: Arc<WorkspaceConfig>, dir: PathBuf, secret_values: Vec<String> }`.

1. Cache hit → return.
2. Commit object missing from the bare repo (a cold replica, or a restart):
   fetch the SHA. If the server refuses want-by-SHA, fall back to fetching
   all heads and tags, then look again. Still missing → `CommitNotFound`
   (permanent: the commit was force-pushed away and is no longer reachable
   on the remote).
3. Checkout to the immutable tree dir (if absent).
4. Load the config (if not cached). A load error from the YAML itself is
   permanent (`PinLoadFailed`); a budget expiry is transient
   (`PinUnavailable`). A `sops`/`vals` failure (an undecryptable SOPS file,
   a failing `vals` reference) is transient too, but bounded, per
   `(ws, commit)` and per replica, in memory: for
   `PIN_SECRET_FAILURE_RETRY_SECS` (30) after one, `ensure` answers the same
   error without loading again (no `sops`/`vals` subprocess per request);
   once the failure has persisted `PIN_SECRET_FAILURE_PERMANENT_AFTER_SECS`
   (3600) since this replica first saw it, it is reported as the permanent
   `PinLoadFailed`, with a message that names the cause. A successful load
   of the commit clears the record. An old commit routinely becomes
   undecryptable for good (key rotation re-encrypts only new commits; a KMS
   key or a `vals` path is removed); without the bound, every read path would
   answer 503 and settlement would wait on it forever.
5. Collect `secret_values` for redaction (§ 9).

`ensure_tree(ws, commit) -> Result<PathBuf>` runs steps 1–3 only (no config
load, no sops/vals). The tarball path (§ 5.4) uses it; everything that needs
definitions uses `ensure`.

**Leases.** `ensure` / `ensure_tree` return an `Arc` handle, and every user
holds it for as long as it reads the config or the checkout dir (a tarball
build holds it until the archive is written). Eviction (§ 10) removes an
entry only when the store holds the last reference (`Arc::strong_count == 1`)
and the entry is not in the keep-set. The checkout dir is deleted only after
the entry has been removed from the map, under the same lock, so an
in-flight user never sees its dir disappear.

Single-flight per `(ws, commit)`. Bare-repo writes (clone, fetch) are
serialised per workspace by a mutex inside the store. Checkouts read objects
only, and run concurrently. Pin loads take a permit from a **separate**
semaphore (`MAX_CONCURRENT_PIN_LOADS`, 4), not the watchers'
`MAX_CONCURRENT_WORKSPACE_LOADS`, with a blocking acquire bounded by the load
budget — so creation-path pin loads never starve the watchers. All git and
YAML work runs on `spawn_blocking`.

Errors carry a typed kind (`PinError::{RefNotFound, CommitNotFound,
NotGit, PinLoadFailed, PinUnavailable}`) so callers classify by
`downcast_ref`, not by message text.

### 5.4 Tarballs

`download_workspace` (`web/worker_api/workspace.rs:44`) gets a new first
branch. A request with `?revision=` for a **configured git workspace** is
answered from the tarball cache. On a miss it calls
`PinStore::ensure_tree(ws, revision)` and builds the tarball from that
checkout dir, plus the library overlay as `build_tarball` does, caching it
under the unchanged key `(ws, revision)`.

This branch runs **before** the live health gate (`get_path` → `None` for
an errored entry, `workspace.rs:53-58`, `workspace/mod.rs:426`). A pin does
not depend on the owner's live load (§ 5.1), so a pinned step whose claim
succeeded must also get its files while `main` fails to load. It 404s only
when the commit does not exist in the repo. A transient `PinUnavailable` (a cold replica
during a git outage) answers **503 with `Retry-After: 5`**.

The worker's pinned download (`ensure_revision`) retries a 503 every 5 s for up
to 12 attempts before failing the step as today. Without this, a tarball
request that lands on a cold replica during an outage would fail the step, even
though the claiming replica has the pin warm.

The only exception to "pin first": when the requested revision **is** the
live one and the live entry is healthy, today's live-dir path is used
unchanged, so the default path keeps its bytes. Folder workspaces have no
history and keep today's code path entirely, including the health gate.

This changes the default path in one way: an **ordinary** job whose revision
fell out of the tarball cache, or whose workspace is now erroring, is served
instead of 404'd. That tarball comes from a clean checkout and has no `.git`
directory (today's live tarballs include `.git` because `build_tarball`
archives the working clone, `workspace.rs:221-247`).

## 6. Data model — migrations `049_git_refs.sql` + `050_git_refs_indexes.sql`

New columns only, all nullable or defaulted; no existing row is rewritten.
The migration also rebuilds two state indexes and adds one partial index on
`job` (see the end of this section).

| Column | Set when | Meaning |
|---|---|---|
| `job.ref TEXT` | The job was created in owner@ref (ref'd `type: task` child, inherited child, ref'd trigger incl. its skipped rows, hook/retry/re-run/restart of a pinned job) | Ref string as written. `job.revision` holds the commit. `ref IS NOT NULL` ⇔ **pinned job** |
| `job.task_folder TEXT` | Every pinned job, from the task's `folder` in the pinned config (NULL = no folder) | The pinned job's ACL folder (§ 7.8) |
| `job_step.action_ref TEXT` | The step's action was resolved through a `ref:`, or is a self-qualified name (`etl.hello`) inherited inside a pinned `etl@R` job (§ 4.3), which stamps the job's own pin | `action_workspace` (now also set when the owner is the job's own workspace) and `action_revision` (the commit) describe the pin |
| `job_step.task_workspace TEXT`, `task_ref TEXT`, `task_revision TEXT` | A `type: task` step whose task resolves to a pin: an explicit `ref:`, or an inherited pin (§ 7.1) | The task owner `T` and its pin, stamped at parent creation. Dispatch reads only these columns and never infers a pin from the parent job |
| `job_step.pin_releases INT NOT NULL DEFAULT 0` | A claim was released because its pin was unavailable (§ 7.2) | Bounds the release-to-ready loop |
| `task_state.ref TEXT`, `workspace_state.ref TEXT` | Snapshot written by a pinned job | Part of the key. `NULL` = unpinned (today's rows) |

**Column naming (implementation).** The SQL and Rust name of `job.ref`,
`task_state.ref` and `workspace_state.ref` is **`git_ref`**, because `ref` is a
Rust keyword and an awkward `FromRow` field. API JSON keeps `ref`.

A `type: task` step carries two owners: the action's owner `O`
(`action_*`) and the task's owner `T` (`task_*`). They are kept in separate
columns on purpose.

State lookups use `ref IS NOT DISTINCT FROM $n`. `idx_task_state_lookup`
(migration 028) and `idx_workspace_state_lookup` (029) are dropped and
recreated with `ref` inserted after the leading key columns. No existing row
is rewritten (D6). A partial index `idx_job_pinned_tasks ON job (workspace,
task_name, task_folder) WHERE ref IS NOT NULL` serves the ACL scope query
(§ 7.8).

**Two migrations, staged for an optional zero-downtime pre-run.** sqlx runs
migrations in one transaction each, at server startup
(`stroem-server/src/main.rs:67`, `stroem-db/src/pool.rs:17`). An index build
inside one holds a `SHARE` lock that blocks writes while it scans.

- **`049_git_refs.sql`**: columns only, all `ADD COLUMN IF NOT EXISTS`.
  Adding nullable or constant-default columns is metadata-only in Postgres
  11+, so this is instant.
- **`050_git_refs_indexes.sql`**: new indexes under **new names**, all
  `CREATE INDEX IF NOT EXISTS`:
  - `idx_task_state_lookup_ref` on `task_state(workspace, task_name, ref,
    created_at DESC)`;
  - `idx_workspace_state_lookup_ref` on `workspace_state(workspace, ref,
    created_at DESC)`;
  - `idx_job_pinned_tasks` on `job(workspace, task_name, task_folder) WHERE
    ref IS NOT NULL`.
  
  Then `DROP INDEX IF EXISTS` of the old `idx_task_state_lookup` /
  `idx_workspace_state_lookup`. Those take a brief exclusive lock and no
  scan.

The default path is a normal deploy, where both migrations run at startup.
The two state tables are empty in practice (D6), so only
`idx_job_pinned_tasks` scans a large table; it indexes no row yet, but the
scan blocks `job` writes for as long as it takes.

For a zero-downtime rollout, migration 050's header documents the manual
pre-run, following migration 009's precedent:
1. Run 049's `ALTER`s.
2. `CREATE INDEX CONCURRENTLY` the three new indexes.
3. `DROP INDEX CONCURRENTLY` the two old ones.
4. Deploy. Both migrations' statements are then no-ops.

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
- `type: task` action **without** `ref` → resolve the task exactly as today,
  relative to the config the action is defined in (the base, § 4.3). If that
  base config is pinned and the task's owner is the base workspace, the task
  inherits the base's pin, and `task_*` is stamped with it. Two cases lead
  here: the action came from a pin (`action_ref` set), or the action is local
  to a pinned job. Otherwise `task_*` stays NULL and dispatch resolves live,
  as today. In particular, a live foreign action `B.run` with an unqualified
  task is NOT pinned, even inside a pinned job.
- A flow step whose action resolves (at its pin) to `type: agent` and carries
  a `ref`, explicit or written on a foreign owner, → 400 "agent actions
  cannot be referenced with `ref` yet". Claim builds agent MCP definitions
  and task-tool schemas from the job's config, not the action owner's
  (`web/worker_api/jobs.rs:788`, `:822`), the same limitation CLAUDE.md
  records for cross-workspace agent actions. Agent steps **inside** a pinned
  job are fine, because the job's config is the pin.

### 7.2 Claim (`claim_job`, `web/worker_api/jobs.rs:429`)

| Step / job | Config used for rendering and action resolution |
|---|---|
| `action_ref` set | `action_workspace`@`action_revision` |
| `action_workspace` NULL, job pinned | `job.workspace`@`job.revision` |
| otherwise | live, as today |

The caller's step `input:` renders in the job's own config (pinned if the job
is pinned), as today for cross-workspace steps. `ClaimResponse.workspace` /
`revision` point at the owner and the commit — the worker's file path is
unchanged. Connection resolution uses the per-claim config set (§ 7.4).

**Transient pin failure at claim.** A claim can land on a replica that never
loaded the pin (HA) or that just restarted. `PinUnavailable` must not fail
the job:

- New primitive `JobStepRepo::release_claim(job_id, step, claim, retry_at)`,
  where `claim = (worker_id, started_at)` is what the claim SQL just wrote.
  It runs in one transaction:
  1. `SELECT status FROM job WHERE id = $job FOR SHARE`. This serialises
     with cancellation, whose job-row `UPDATE` takes the row lock
     (`JobRepo::cancel`, then `cancel_pending_steps`,
     `stroem-db/src/repos/job_step.rs:949`, which cancels only
     `pending`/`ready`/`suspended` rows and leaves `running` ones for the
     worker to drain).
  2. `SELECT … FROM job_step WHERE … AND status = 'running' AND worker_id =
     $w AND started_at = $s FOR UPDATE`. No row → `NotApplied`: the step
     moved on (recovery, completion). The claim returns "no work" and does
     nothing else.
  3. Decide, still holding both locks:
     - **Job terminal** (cancelled meanwhile) → `Cancelled`. Set this step
       `cancelled` with `completed_at`, **and** run `cancel_pending_steps`
       for the job in the same transaction. Cancellation is two separate
       calls (`settlement/mod.rs:684`, `:694`), and without this the step's
       settlement could win terminal handling while siblings are still
       `ready`.
     - **`pin_releases + 1 > MAX_PIN_RELEASES`** (30, ≈ 5 minutes at 10 s) →
       `CapReached`. Nothing is written; the step stays `running` under
       this claim.
     - **Otherwise** → `Released`. Set `ready`, clear `worker_id` and
       `started_at`, `ready_at = now`, `retry_at = now + 10 s`,
       `pin_releases += 1`.

  The handler acts on the outcome:
  - `Released`: claim returns "no work".
  - `Cancelled`: `Settlement::step_settled`, so the drain gate and terminal
    handling run with no live sibling left.
  - `CapReached`: `fail_claimed_step` with `expected_claim = (w, s)`, so the
    failure applies only while the row is still this claim.
  - `NotApplied`: nothing.

  A released step is not a failure: `retry_attempt` and `retry_history` are
  untouched. `claim_ready_step` already honours `retry_at`. Resetting
  `ready_at` keeps the unmatched-step sweep (which measures from `ready_at`,
  `job_step.rs:1109`) from counting the time the step spent claimed.
- Recovery phases that fail a **running** step they selected earlier (stale
  worker, step timeout; `recovery.rs:69`, `:120`) pass the claim identity
  they observed to a new optional `fail_or_retry` guard, `expected_claim:
  Option<(Uuid, DateTime)>`. A row whose `(worker_id, started_at)` no longer
  matches is `NotApplied`. Today `fail_or_retry` accepts any status when its
  expected list is empty (`job_step.rs:747`); a released and reclaimed step
  would otherwise be failed on the strength of the previous attempt. The
  other `fail_or_retry` callers are unchanged.
- The job log gets `[pin] {ws}@{ref} ({short sha}) not available yet on this
  server, retrying: {scrubbed error}` (`_server` step).
- Backstops: the job timeout, when one is set; and the `CapReached` outcome
  above. Without the cap, a job with no timeout would loop forever on a
  permanently unreachable remote.

Permanent pin errors (`RefNotFound`, `CommitNotFound`, `PinLoadFailed`) go
through `fail_claimed_step` (`jobs.rs:364`) as any claim-time failure does.
`fail_claimed_step` always passes the claim identity it holds as
`expected_claim`.

**Owner-side render errors are withheld at claim.** This is today's dispatch
policy (`settlement/dispatch.rs:121`, CLAUDE.md § Secrets in logs) extended to
claim. It applies to every claim whose step's action owner differs from
`job.workspace`, pinned or live.

An error raised while rendering the **owner's** templates is classified by
origin, not by message, using a typed marker like `job_creator::OwnerSideRender`
on the `anyhow` chain. That covers:
- the owner's action input defaults and its connection resolution
  (`prepare_step_action_input`);
- the action body (`script` / `cmd` / `env` / `args` / `image` /
  `manifest`) rendered with the owner's secrets.

Such an error is persisted, logged to the job and returned to the worker as a
fixed, value-free sentence: "rendering action '{name}' of workspace
'{owner}' failed; details withheld". The full scrubbed chain goes only to the
server log (`tracing::error!`).

The caller's own step `input:` (rendered in the caller's context) and
structural errors (action missing, pin errors) stay visible after scrubbing,
as today. Own-workspace refs cross no boundary and are unaffected.

This closes, for refs and for today's cross-workspace actions alike, the gap
where a filter chain wraps an owner secret in an encoding the scrubber cannot
match (`workspace_set.rs:163`). Refs make that gap reach secrets that exist
only on an unreviewed branch.

### 7.3 Settlement

`Settlement::resolve(job)` (`settlement/mod.rs:135`) reads
`config_for(job.workspace, pin of job)`. Because a pinned config is
immutable, the cascade, hooks, retry and approvals of a pinned job all see
one commit.

| Derived job | Ref + commit |
|---|---|
| `type: task` child with `task_*` stamped (explicit or inherited, § 7.1) | `task_ref` / `task_revision` |
| `type: task` child without `task_*` | Unpinned; resolved live, as today (a same-workspace child still inherits the parent's `revision` for files, unchanged) |
| Hook job of a pinned job (`fire_single_hook`) | Source job's `ref` + `revision`; the hook definition comes from the pinned config |
| Task-level retry (`create_retry_job`, `settlement/retry.rs:108`) | Failed job's `ref` + `revision` |
| Re-run / Restart of a pinned top-level job | **Re-resolves** `job.ref` (today they take the current revision; for a ref that is its current commit) — see below |
| Agent task-tool child of a pinned job (`agent_task_tool`, `jobs.rs:1073`) | Parent's `ref` + `revision`; the tool's task is looked up in the pinned config |

`dispatch::handle_task_steps_pass` picks the task's config from `task_*` if
set, else live (today). It never infers a pin from the parent job.

**A pinned job whose pin cannot be loaded must not strand.** `Settlement::resolve`
logs a `PinUnavailable` for a pinned job and returns `Ok(None)`. One case is a
step completing on a cold replica during a git outage. Today nothing would
re-enter `advance` afterwards, so the next steps would stay `pending` forever.

A recovery phase fixes this. `readvance_stalled_pinned_jobs` runs on every
leader sweep and selects

```sql
SELECT job_id FROM job j
WHERE status IN ('pending','running') AND git_ref IS NOT NULL
  AND NOT EXISTS (SELECT 1 FROM job_step s
                  WHERE s.job_id = j.job_id
                    AND s.status IN ('ready','claimed','running','suspended'))
  AND (status = 'running'
       OR EXISTS (SELECT 1 FROM job_step s
                  WHERE s.job_id = j.job_id
                    AND s.status IN ('completed','failed','skipped','cancelled')))
```

A job stays `pending` until a worker calls `/start`, so a step that fails at
claim (the release cap, a permanent pin or render error) or whose tarball
download is exhausted leaves it `pending`; the `advance` after that failure
may run on the very replica that cannot load the pin. A `pending` job is
listed only once it has a terminal step: one with none is a job whose
creation-time init has not promoted its first steps yet, and is never
advanced concurrently with that init.

It calls `Settlement::advance` for each job, with one heartbeat per job
(CLAUDE.md § Health Check). `advance` is idempotent, so a job that is
merely between events loses nothing.

A **permanent** pin error (`NotGit`, `CommitNotFound`, `PinLoadFailed`) in
`Settlement::resolve` behaves differently from a transient one. It does not
return `Ok(None)`. It settles the non-terminal job `failed` with a
`[pin] {ws}@{ref} ({short sha}) cannot be loaded` server-log line, so the
re-advance phase can never loop on it.

**Re-run and restart of a pinned source.** Both reject non-top-level sources
(`is_top_level_job`, `web/api/jobs.rs:629`), so this concerns pinned jobs
created by a ref'd trigger. Today both look the task up in the **live**
config first: re-run through the execute route's task check
(`web/api/tasks.rs:450`, `:479`), restart through its plan
(`web/api/jobs.rs:700`, `:713`). A task that exists only at the ref would
404.

When the source has `job.ref`, both instead:
1. Re-resolve the ref (`PinStore::resolve`) and `ensure` the new pin, before
   any task check.
2. Look the task up in that pinned config, and require `Run` on the
   folder it declares there — the new job's `task_folder` — with the execute
   route's mapping (Deny → 404 "Task", View → 403 "View-only access"),
   `dry_run` restart included. The source job's own `task_folder` is checked
   first, before anything about the source is revealed. Restart computes its
   `RestartPlan` (`restart::compute_restart_set`) against the pinned flow
   and seeds carried rows in the existing creation transaction
   (`job_creator.rs:521`).
3. Create the new job with `job.ref` (same string), the new commit and
   `job.task_folder` from the new pin.

Carried outputs come from the source commit and the restart set runs the new
one. That is today's documented "revision drift" for restart, now made
explicit by the commit. The execute route accepts the pinned path only via
`source_job_id`; it is not a per-run ref override (§ 16).

### 7.4 `config_for`

One function, `WorkspaceManager::config_for(ws, pin: Option<&PinRef>) ->
Result<ConfigHandle>`: `None` → today's live `get_config`; `Some` →
`PinStore::ensure`. `ConfigHandle` holds the lease (§ 5.3).

**The job's own config** (`config_for(job.workspace, pin of job)`) replaces
`get_config(job.workspace)` at every site that reads a job's definitions:

- `Settlement::resolve` (`settlement/mod.rs:135`);
- `claim_job` (`web/worker_api/jobs.rs:429`);
- `agent_task_tool` (`:1073`, `:1105`);
- job detail step ordering (`web/api/jobs.rs:383`);
- `dispatch::init` / `fire_initial_suspended_hooks` (`settlement/dispatch.rs:861`,
  `:750`). These currently take a config from the creating caller (e.g.
  `web/hooks.rs:146` passes the webhook's defining config). They now derive
  it from the created job row. For every existing creation path that is the
  same config the caller passed; for a cross-workspace or pinned trigger
  target it is the target's.

**Role-scoped connection lookup.** Connection resolution identifies configs
only by workspace name today (`workspace_set.rs:13`,
`stroem-common/src/template.rs:877`). That breaks once two **roles** of one
resolution are the same workspace at different commits. Examples: a pinned
job `W@X` calling `action: import, ref: R2`; or a `type: task` step whose
action owner `O` and task owner `T` are one workspace pinned at two commits.
A name-keyed overlay lets one commit shadow the other.

The lookup handed to the resolver therefore becomes role-scoped:

```
RoleScope {
    caller:       ConfigHandle,          // A: the job's own config (pinned or live)
    action_owner: Option<ConfigHandle>,  // O: the step's action pin / live owner
    task_owner:   Option<ConfigHandle>,  // T: the step's task pin / live owner (type: task)
    others:       WorkspaceSet,          // every other workspace, live
}
```

**Rule.** Every existing resolution rule is kept as written. Each "config of
workspace X" it consults becomes the handle of the role that the rule names.
Concretely:

| Lookup | Role handle |
|---|---|
| Bucket `C`: caller's rendered step `input:` (connection names) | `caller` |
| Bucket `D`: action `input` defaults (`merge_action_defaults`, `prepare_action_input_cross`) | `action_owner` |
| The task's own defaults, and the task's **input schema** (which fields are connection-typed, their types) | `task_owner` |
| The action's input schema at claim | `action_owner` |
| Caller-first, then owner-if-shared fallback for a caller-supplied name | `caller`, then the role whose schema the field belongs to. The `shared` gate applies only when that role's workspace ≠ the caller's |
| Qualified `ws.conn` / `type: ws.type` inside a bucket | The bucket's role handle when `ws` names that role's workspace, else `others` |

Sites, each building `RoleScope` from what it has stamped:

| Site | Roles used |
|---|---|
| Creation pre-check, flow-step action (`precheck_literal_connection_inputs`, `job_creator.rs:790`) | caller + action owner |
| Creation pre-check, `type: task` (`precheck_task_step_literals`, `job_creator.rs:815`) | caller + task owner |
| Claim preparation (`web/worker_api/jobs.rs:615`) | caller + action owner |
| `type: task` dispatch (`template::resolve_task_input_by_provenance`, `template.rs:961`; `settlement/dispatch.rs:322`, `:402`) | caller + action owner + task owner |

When no role is pinned, every handle is today's live config and behaviour is
unchanged.

**Per-job redaction set.** These all redact with the live set's values plus
`secret_values` of **every** pin of the job's **redaction closure** (below):
`job.ref`, and each step's `action_ref` and `task_ref`, of every job in it:
- job detail (`web/api/jobs.rs:491-494`);
- the sync webhook response (§ 7.5);
- MCP `get_job_status` (`mcp/tools.rs:577`), which today returns job output
  and step `error_message` raw although job detail redacts them;
- worker detail's recent steps (`web/api/workers.rs:134-148`), which today
  returns `error_message` raw. The page lists steps of many jobs, so it
  builds one redaction set per distinct job;
- every branch of the webhook job-status poll (`web/hooks.rs:305-311`,
  `:360-390`, `:457-467`), which today returns `job.output` raw;
- `fail_claimed_step` and `fail_task_step`.

Redaction applies to the **whole response object**, not to named source
fields. Job detail copies `output.approval_message` into a separate step
field (`web/api/jobs.rs:348-355`); approval messages are rendered with the
job's secrets (`settlement/dispatch.rs:583-589`, `:684-717`). Today's
redactor visits only `input` / `output` / `raw_input` (`web/api/jobs.rs:532-559`)
and would miss that copy. After this change it walks every string in the
serialised step entries and in the job object. The walk skips identifier
keys (`redaction::JOB_IDENTIFIER_KEYS`, `STEP_IDENTIFIER_KEYS`), so a short
secret value cannot mangle a link. It skips the `child_jobs[]` summaries
whole, which is safe only while they hold identifiers (§ 7.8).

**Redaction closure** (Task 22 review). Content is copied between jobs:
- **up:** a child's output settles into its parent's `type: task` step
  (`Settlement::propagate`). The parent references the child's own pin
  (`task_ref`), but not the pins of the child's steps;
- **down:** a parent renders its own values into a child's input. A pinned
  parent can do this for a live cross-workspace child (bucket `C` is
  rendered in the caller's context);
- **across:** a parent step renders a sibling child's output into another
  child's input;
- **into a hook:** a hook payload quotes its source's step errors
  (`hook.error_message`, `hook.failed_steps[]`,
  `settlement/hooks.rs::build_hook_context`), and the rendered hook input
  becomes the hook job's input. The hook job's only pin is its source's
  `job.ref`; its own step has none;
- **into a restart:** a restart's carried rows copy the source's step
  `output` / `error_message` (`JobStepRepo::seed_steps_tx`) into rows
  stamped with the pins of the commit the restart runs at;
- **into a task retry:** a retry job replays the failed job's `input`
  (`settlement/retry.rs::create_retry_job`), so a retried hook job carries
  its source's payload;
- **into a re-run:** a re-run replays its source's `raw_input` (the `••••••`
  sentinels of `secret: true` and connection fields). A task retry's
  `raw_input` is the failed job's RESOLVED input — secret-rendered defaults
  and resolved connection properties included — so a re-run of a retry, or
  of a re-run of one, carries values of the retried commit into a job pinned
  at the re-resolved one.

So the set of the job's own pins alone misses a value copied in from
another job. Rule: a job's redaction set is the live values plus
`pin_redaction_values` of every distinct `(workspace, commit)` pin
referenced (`job.ref`, and each step's `action_ref` and `task_ref`) by any
job in the **whole tree of every job in its source lineage**:
- **Source lineage:** the job, plus the jobs it was made from:
  - from a `source_type = 'hook'` job, the job that fired it
    (`source_job_id`, or the UUID prefix of `source_id` on a pre-048 hook
    row);
  - from a `source_type = 'restart'` or `'rerun'` job, its source
    (`source_job_id`) — always, not only when the source is a retry: a
    re-run of a re-run copies the same values;
  - from a task retry, the job it re-runs (`retry_of_job_id`, always the
    root original).

  The walk goes up `parent_job_id` too, so a child of a hook, restart,
  re-run or retry job reaches that job's source.
- **Whole tree:** for each lineage job, its root (walking `parent_job_id`
  up) and every descendant of that root.

**Bounds fail closed.** The walk is bounded:
- `MAX_TASK_DEPTH` levels up and down;
- `MAX_HOOK_CHAIN_DEPTH` hook links;
- `redaction::MAX_SOURCE_LINEAGE_HOPS` (32) restart and re-run links,
  counted together. Restart and re-run chains have no cap of their own;
- `redaction::MAX_RETRY_LINEAGE_HOPS` (33) retry links. One per retry
  generation, more only when restarts or re-runs and retries interleave;
- `redaction::MAX_REDACTION_CLOSURE_JOBS` (20 000) jobs per walk.

A bound never cuts silently. A refused edge makes the closure
**truncated**: a parent or a child at the depth limit, or a hop past its
cap. So does a walk with more jobs than the node cap. A truncated closure
answers `MaskAll`: retrying would hit the same bound.

Every bound is reachable:
- restart and re-run chains are otherwise unbounded;
- agent task-tool children skip the `MAX_TASK_DEPTH` check;
- `hook_chain_depth` fails open.

The depth bound equals the creation cap: the deepest tree `type: task`
dispatch allows (a child with 10 ancestors) fits exactly. The recursion
reads at most `max_jobs + 1` rows of each walk, so the node cap also
bounds the work done on every outlet call.

One recursive query reads the distinct pins and the truncation flag
(`JobRepo::redaction_closure_pins` → `RedactionClosure::{Pins, Truncated}`,
bounds `redaction::CLOSURE_BOUNDS`). It is merged with the job's own pins
from the rows the outlet shows (`redaction::closure_pins`). No copy path is
special-cased.

**Short-circuit.** When no row anywhere references a pin, every closure's
pin set is empty. A pinned row is `job.git_ref IS NOT NULL`, or a step's
`action_ref` / `task_ref`. In that case the redaction set is the live
values alone, and the walk is skipped. Truncation cannot hide a pin that
does not exist.
- `JobRepo::any_pinned_rows` makes two `EXISTS` probes, on
  `idx_job_pinned_tasks` and the partial index `idx_job_step_pinned`
  (migration 050).
- It runs after the outlet has read the rows it will show. Any pinned row
  whose values could appear in them already existed by then.
- A deployment that never uses refs pays two index probes per outlet call,
  not a tree walk.

**Memo.** Worker detail redacts up to 50 jobs per request through one
`RedactionMemo`. The short-circuit probe, each job's closure (by job id) and
each pin's values (by `(workspace, commit)`, failures included) are
computed once per request.

Fail-closed is otherwise unchanged, over the whole closure:
- a transient pin failure anywhere in it answers 503;
- a permanent one masks everything;
- a closure that cannot be read (a DB error) answers 503.

The price: an outlet loads every pin of the tree, so a cold pin anywhere in
it can 503 an otherwise unrelated job's detail.

**Known gaps** (not covered by the closure):
- An event-source emitted job has no link to its consumer job: the emitted
  JSON becomes the target job's input with no lineage column.
- Retention deletes a source before its receivers. The lineage foreign keys
  are `ON DELETE SET NULL`, so in that window a receiver's closure can lose
  the deleted source's pins.
- Agent task-tool children skip the `MAX_TASK_DEPTH` check. A tree deeper
  than the cap is now handled by the depth bound, which fails closed.

These outlets were found by grep and are checked again by the audit task in
§ 7.9. The audit at `324f0b1` found no outlet route missing from this list.
Its review then found the copy paths above, which the job's own pins
missed. The redaction closure covers them.

Step `error_message` can carry a failing script's stderr
(`stroem-worker/src/poller.rs:745`, persisted at `web/worker_api/jobs.rs:939`).

If a referenced pin cannot be ensured (`PinUnavailable` on a cold replica),
job detail, the sync webhook and MCP status **fail closed** with 503 / an MCP
error "redaction set unavailable, retry" (the webhook body still carries
`job_id`).

Worker detail fails closed **per row**: an affected step's `error_message`
is replaced by `••••••`, so one cold pin does not fail the whole page.

No path answers with a redaction set that is missing a pin.

Other entry points: `handle_task_steps_pass` / `resolve_task_ref` (via
`task_*`), the state endpoints (§ 7.6) and `download_workspace` (§ 5.4).

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
5. Create a top-level job in `T` with `job.ref`, `job.revision` and
   `job.task_folder` (or `T`'s current revision when there is no `ref`). A
   `skip`-policy skipped row (`scheduler.rs:414`, `JobRepo` at
   `stroem-db/src/repos/job.rs:308`) is written for the same resolved target
   `{T, task, ref, commit, task_folder}`, never for the defining workspace.
   Both creation paths receive that resolved target as one value.

Any failure in steps 1–3 logs `Trigger '…' MISSED: …` with no side effects —
no `cancel_previous`, no skipped row — the rule already used for an
unavailable workspace. `triggers: false` follows the **defining** workspace.

**Webhooks.** Today `hooks::find_webhook_trigger` captures the trigger's
task, defaults, secret and mode **before** `force_refresh`, and the handler
creates the job from those captured values with no revalidation
(`web/hooks.rs:48`, `:67`). With refs, that would let a refresh that changed
or removed a webhook's `ref` still run the old release. New order:

1. Match the webhook by name, and authenticate against the **cached**
   definition's secret. This pre-check is today's behaviour. It is kept so
   that an unauthenticated caller can never trigger a git refresh.
   Consequence: a caller holding only a newly rotated-in secret gets 401
   until the server has loaded that secret (watcher poll or another refresh).
2. If its definition has `force_refresh`, reload. If the workspace errored on
   reload (no config), answer 500, never 404.
3. **Match it again** in the refreshed config. If it is gone, answer 404.
4. Authenticate again, against the **fresh** definition's secret.
5. Resolve the target (steps 1–2 above) from the fresh definition.

Errors map as in § 8 (400/500; MISSED is a scheduler term). The initial
`on_suspended` hooks and approvals of the created job come from the job's
own config (§ 7.4), not from the webhook's defining workspace.

**Webhook output** is redacted with the job's per-job redaction set
(§ 7.4), failing closed. That covers both sync-invocation branches
(`web/hooks.rs:156`, `:177`) and every branch of the async job-status poll
(`:305-467`). Today it returns `job.output`
unredacted for every job. Refs would extend that to secrets that exist only
at a ref, so the fix applies to all sync webhook responses.

### 7.6 State

- Render context: `render_context::latest_snapshots` (`render_context.rs:50`)
  takes the job's ref and reads `TaskStateRepo::get_latest(ws, task, ref)` /
  the workspace-state equivalent.
- **The state coordinates `(workspace, task, ref)` are the job's own, and the
  server derives them from the job, never from the client.** Today the worker
  sends `ClaimResponse.workspace` (the step's action **owner**) with the
  job's task name (`stroem-worker/src/poller.rs:367`, `:427`, `:620`). A
  cross-workspace step therefore reads `(owner, caller task)`, and its upload
  fails the server's workspace check (`web/worker_api/state.rs:400`).
  - Uploads (`POST /worker/state/{ws}/{task}/{job_id}` and the global-state
    equivalent) already load the job (`state.rs:389`). They now write to
    `(job.workspace, job.task_name, job.ref)` and no longer require the path
    `{ws}` / `{task}` to match: the path coordinates are ignored once the
    job is known.
  - Downloads (`GET /worker/state/{ws}/{task}`, `GET
    /worker/global-state/{ws}`) gain an optional `?job_id=`, which the worker
    sends from the claim. With it, the server reads the job's coordinates the
    same way. Without it (an old worker), the path coordinates and the
    `NULL` partition are used, which is today's behaviour.
  - This also fixes today's cross-workspace state keying, as a consequence of
    deriving everything from the job.
- Manual uploads (`POST /api/workspaces/{ws}/tasks/{task}/state`, `…/state`)
  are **not** ref-aware in v1. They write and merge the `NULL` partition
  exactly as today (`web/api/state_upload.rs:339`, `:365`, `:581`). State is
  unused today, so partitioned manual upload is a follow-up (§ 16). The state
  list endpoints return `ref` per snapshot.
- Retention (`max_snapshots`) prunes per `(workspace, task, ref)`.

### 7.7 Template and hook metadata

`render_context::job_context` (`render_context.rs:216`) adds `ref`
(`{{ job.ref }}`, `""` for unpinned jobs) next to `revision`; hooks get
`hook.ref`. `{{ job.revision }}` of a pinned job is its commit.

### 7.8 ACL

ACL keeps keying on the workspace name (§ 4.5). Only the **folder** half of
the task path `{folder}/{task}` needs a rule for pinned jobs. Today the
folder comes from the live config, in two places:

- **Single-job checks:** `check_job_acl` (`web/api/jobs.rs:1091`, `:1108`)
  and MCP's per-job check (`mcp/tools.rs:291`).
- **Lists:** REST `resolve_acl_scope` (`web/api/jobs.rs:1141`) and MCP's own
  `resolve_acl_scope` (`mcp/auth.rs:155`, `:178`). Both build allowed
  `(workspace, task)` **pairs** from live tasks. The list, count and
  status-count SQL authorise on that pair alone (`stroem-db/src/repos/job.rs:1070`,
  `:1134`), and so does MCP `list_jobs` (`mcp/tools.rs:710`, `:726`).

**Rule.**
- An **unpinned** job keeps today's rule: its folder is the live task's.
- A **pinned** job's folder is always its own `job.task_folder`, the folder
  its commit declared. The live config is never consulted, even when a task
  of the same name exists live.

This is consistent with "a pinned job runs that commit". It also avoids
conflating two refs of one task that declare different folders.

- **One helper.** `acl::job_task_path(state, &JobRow) -> String` returns
  `{task_folder}/{task}` for a pinned job and the live-folder path for an
  unpinned one. No pin load is needed. `check_job_acl` changes signature to
  take the `JobRow` (it takes `(workspace, task_name)` strings today) and
  calls the helper. Every **job-scoped** read path goes through it. The
  inventory at `b367b6c` came from a grep for `make_task_path`, `.folder`
  and `check_job_acl` under `web/` and `mcp/`. The § 7.9 audit re-checked it
  at `324f0b1` against every route and MCP tool, and added the sync webhook
  response and the `child_jobs[]` row:

  | Path | Today | After |
  |---|---|---|
  | Job detail, cancel, approve, restart source, re-run source (`web/api/jobs.rs:304`, `:577`, `:667`, `:830`; `web/api/tasks.rs:507`) | `check_job_acl(ws, task)` | `check_job_acl(&job)` |
  | REST logs (`web/api/logs.rs:84`) | `check_job_acl(ws, task)` | `check_job_acl(&job)` |
  | Artifacts list / download (`web/api/artifacts.rs:115`, `:153`) | `check_job_acl(ws, task)` | `check_job_acl(&job)` |
  | WebSocket log stream, backfill and live (`web/api/ws.rs:103-136`) | inline live-folder derivation (`:119`) | `job_task_path(&job)` |
  | Worker detail, recent steps (`web/api/workers.rs:98-148`) | inline live-folder derivation per step (`:106`) | per step row: the step query joins `job.ref` and `job.task_folder`, and each row uses `job_task_path` |
  | MCP per-job checks: status, logs, cancel, `list_artifacts`, `get_artifact` (`mcp/tools.rs:291`, `:816-824`, `:864-872`) | live folder | `job_task_path(&job)` |
  | Task duration stats (`GET /api/workspaces/{ws}/tasks/{name}/stats`, `web/api/tasks.rs:363-427`; queries `stroem-db/src/repos/job.rs:1234-1244`, `:1267-1278`, `job_step.rs:1279-1302`) | live folder for the task; the queries select every job of that name | **pinned jobs are excluded** (`AND ref IS NULL` in all three queries). Stats describe the live task, and a release's runs, with their different flows, are not its runs. That also removes any need for the pinned-folder predicate there |
  | Webhook job status (`GET /hooks/{name}/jobs/{job_id}`, `web/hooks.rs:305-467`) and the sync webhook response (`/hooks/{name}` with `mode: sync`, `web/hooks.rs::sync_response`) | **webhook authentication** (`:323-348`), not task ACL | unchanged: an explicit exception, since the caller holds the webhook's secret. Its output is redacted (§ 7.4) |
  | REST + MCP lists and counts | live pairs | per-job predicate (below) |
  | `child_jobs[]` summaries in job detail (`web/api/jobs.rs::get_job`), and the lineage ids `parent_job_id` / `source_job_id` / `retry_of_job_id` / `retry_job_id` | the parent's `check_job_acl` | unchanged: the parent's ACL, **no per-child filter**. A summary holds identifiers only: child id, workspace, task name, status, `created_at`, `ref`, `revision`. All of it already follows from the parent. The parent's own flow names the child's task, and its step stamps `task_workspace` / `task_ref` / `task_revision`. The child's result settles into that step: `output` on success, `Child job {id} failed` as `error_message` (`Settlement::propagate`). Filtering would hide nothing. The child's own input, steps, logs and artifacts stay behind `check_job_acl(&child)`. Redaction skips `child_jobs` whole (`STEP_IDENTIFIER_KEYS`), so a content field must never be added to it. Test: `read_path_audit_test.rs` |

  Task-scoped paths are unchanged: task list and detail, execute, triggers,
  workspaces, manual state upload. They concern live tasks.

  CLAUDE.md gains the rule: a new read path that exposes a job must authorise
  with `check_job_acl(&job)` / `job_task_path`, never by looking up a task's
  folder in the live config.
- Lists authorise **per job**, in REST and MCP alike. The scope becomes:
  - `live_pairs`: as today, for unpinned jobs;
  - `pinned_triples`: the ACL rules evaluated over `SELECT DISTINCT
    workspace, task_name, task_folder FROM job WHERE ref IS NOT NULL`
    (served by `idx_job_pinned_tasks`, § 6).
  
  The list, count and status-count queries filter on `(ref IS NULL AND
  (workspace, task_name) IN live_pairs) OR (ref IS NOT NULL AND (workspace,
  task_name, COALESCE(task_folder, '')) IN pinned_triples)` **in SQL**, before
  `ORDER BY` / `LIMIT` / `OFFSET`. A user allowed one ref's folder therefore
  never sees another ref's jobs under a denied folder.
- MCP `list_jobs` stops filtering in Rust after `LIMIT` (`mcp/tools.rs:708`,
  `:726`). Today that lets denied recent jobs fill the page and hide older
  permitted ones. It now calls the same SQL-side `JobRepo::list_with_acl`
  path as REST, with the predicate above.
- The execute-time ACL check (`Run` on the task) is unchanged for ordinary
  executes. A re-run or restart of a pinned source checks the source's
  `task_folder`.

### 7.9 Read-path audit (implementation task)

The § 7.8 table and the § 7.4 outlet list were built by grep. Codex rounds 5
and 6 each found paths missing from them. So the design does not rely on the
lists being complete; the implementation plan carries a dedicated task:

1. Enumerate every route in `web/api/`, `web/hooks.rs`, `web/worker_api/`
   (worker-facing paths: none returns data to a user) and every MCP tool
   that returns job-scoped data: job or step rows, `error_message`, job or
   step output, logs, artifacts, approval messages, hook payloads.
2. Classify each one as job-scoped ACL (§ 7.8 helper), list/count
   predicate, task-scoped, or an explicit exception (webhook status).
   Classify it as redaction outlet or not (§ 7.4).
3. Add one integration test per job-scoped path: a pinned job in a denied
   folder is denied, and a ref-only secret is masked.

The audit's output updates § 7.8 / § 7.4 in this spec, and the
implementation review checks it.

**Audit run at `324f0b1`.** Every route registered in `web/mod.rs`,
`web/api/mod.rs`, `web/hooks.rs`, `web/worker_api/mod.rs` and `oauth/mod.rs`
was enumerated, along with all ten MCP tools, and each was classified. No
state list endpoint exists. The audit found two entries missing from § 7.8:
the sync webhook response and the `child_jobs[]` summaries. It found no
missing § 7.4 outlet route. Every job-scoped path already had its test from
the task that changed it.

The audit's review found one hole. A value can be copied INTO a job from
another job, and the job's own pins did not cover it. The copies run:
- up (a child's output);
- down and across (a parent's or sibling's values rendered into a child's
  input);
- into a hook payload;
- into a restart's carried rows;
- into a task retry's replayed input.

The § 7.4 redaction closure, the whole tree of every job in the source
lineage, fixes it. Its bounds fail closed, and a global short-circuit
skips it when no pin exists anywhere. New tests cover `child_jobs[]`, every
copy direction, truncation and the short-circuit.

Hook jobs: a single-step hook job is named `_hook:{action}`. That name
matches no task, and a pinned hook job stamps no `task_folder`, so it is
authorised as task `_hook:{action}` at the root folder. This is
pre-existing. A rule that grants the root `_hook:*` grants View on every
hook payload of that workspace, and so on the source errors it quotes, even
when the source job's own folder is denied. A `type: task` hook job is an
ordinary job of its task and is authorised by that task's folder.

| Class | Routes and tools | Outlet (§ 7.4) | Tested by |
|---|---|---|---|
| Job-scoped (`check_job_acl(&job)` / `job_task_path`) | `GET /api/jobs/{id}`; `POST /api/jobs/{id}/cancel`, `/restart`, `/steps/{step}/approve`; `GET /api/jobs/{id}/logs`, `/steps/{step}/logs`, `/artifacts`, `/artifacts/{name}`; the WebSocket `/api/jobs/{id}/logs/stream`; the re-run source of `POST …/execute`; MCP `get_job_status`, `get_job_logs`, `cancel_job`, `list_artifacts`, `get_artifact` | job detail and MCP `get_job_status`. Logs are not redacted (§ 16) | `git_refs_read_paths_test.rs`: deny in `pinned_job_in_denied_folder_is_denied_on_every_rest_path`, `…_on_the_websocket` and `…_over_mcp_and_list_paginates_after_acl`; mask in `job_detail_masks_ref_only_secret_in_every_field` and `mcp_get_job_status_masks_ref_only_secret_and_fails_closed`. Re-run source: `pinned_rerun_restart_test.rs::pinned_rerun_authorises_against_the_source_task_folder` |
| Job-scoped per row | `GET /api/workers/{id}` (recent steps) | yes, per row | `pinned_job_in_denied_folder_is_denied_on_every_rest_path`, `worker_detail_masks_error_and_fails_closed_per_row` |
| List / count predicate | `GET /api/jobs`, `GET /api/stats`, MCP `list_jobs` | no (metadata only) | the same deny tests |
| The parent's ACL | `child_jobs[]` and the lineage ids in job detail | identifiers, skipped | `read_path_audit_test.rs::child_job_summary_in_job_detail_is_identifiers_only_and_the_child_stays_job_scoped` |
| Content copied between jobs (redaction closure, § 7.4) | a child's output in its parent's step (up); a pinned parent's values in a live child's input (down); a hook payload in the hook job's input; a restart's carried rows; a task retry's replayed input; a re-run's replayed `raw_input` | yes: the receiving job's set covers the whole tree of every job in its source lineage; a truncated closure masks everything; no pin anywhere → the live set alone | `read_path_audit_test.rs`: `parent_job_detail_masks_a_secret_of_its_childs_step_pin`, `child_job_detail_masks_a_secret_its_pinned_parent_rendered_into_its_input`, `hook_job_detail_masks_a_secret_of_its_sources_step_pin`, `retried_hook_job_detail_masks_a_secret_of_its_hook_source`, `job_detail_masks_everything_when_its_redaction_closure_is_truncated`; `pinned_rerun_restart_test.rs::restart_masks_a_carried_secret_of_the_source_commit`, `rerun_of_a_retry_masks_a_secret_of_the_retried_commit`; stroem-db `git_refs_test.rs`: `redaction_closure_pins_cover_the_whole_tree_of_the_source_lineage_and_fail_closed_at_a_bound`, `any_pinned_rows_sees_a_pinned_job_or_a_pinned_step` |
| Job-scoped, hook job (pre-existing) | single-step hook jobs on every job-scoped path: task path `_hook:{action}`, root folder | as any job | the job-scoped deny tests (the rule is the same `check_job_acl`) |
| Task-scoped (live), pinned jobs excluded | `GET /api/workspaces/{ws}/tasks/{name}/stats` | no | stroem-db `git_refs_test.rs::duration_stats_exclude_pinned_jobs` |
| Task-scoped (live) | workspaces and refresh, task list and detail, triggers, execute (not a re-run), manual state upload; MCP `list_workspaces`, `list_tasks`, `get_task`, `execute_task` | no | unchanged |
| Explicit exception: webhook auth | the sync response of `/hooks/{name}`, `GET /hooks/{name}/jobs/{job_id}` | yes, every branch | `sync_webhook_masks_ref_only_secret`, `sync_webhook_fails_closed_with_job_id`, `webhook_status_poll_masks_ref_only_secret_in_every_branch`, `webhook_status_poll_fails_closed_with_job_id` |
| Worker token, not user-facing | `/worker/*` | no | state partitions: the Task 19 `state_*` / `global_state_*` tests |
| No job data | `/livez`, `/healthz`, `/healthz/detail`, `/metrics`, `/api/config`, `/api/auth/*` (incl. `api-keys`, `oidc`), `/api/users*`, `/api/groups`, `GET /api/workers`, `/api/oauth/*`, `/oauth/*`, `/.well-known/*` | no | none needed |

## 8. Errors and classification

`classify_execute_error` (`web/api/mod.rs:393`) gains a typed check (via
`downcast_ref::<PinError>()`, before the phrase tiers):

| Condition | Execute API | Trigger | At claim |
|---|---|---|---|
| Unknown workspace, folder owner, library item + `ref`, invalid ref syntax | 400 | MISSED | — (never created) |
| `RefNotFound`, `CommitNotFound` | 400 | MISSED | permanent → `fail_claimed_step` |
| Name missing at that commit ("has no action/task … at ref") | 400 | MISSED | permanent |
| `PinLoadFailed` (YAML at that commit does not load; sops/vals failing for an hour, § 5.3) | 400 | MISSED | permanent |
| `PinUnavailable` (ls-remote/fetch failure with no cached listing, budget, sops/vals for under an hour) | 500 | MISSED | transient → `release_claim` (§ 7.2) |
| Ref'd agent action (§ 7.1) | 400 | MISSED | — (never created) |

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
  These are assembled as the per-job redaction set (§ 7.4), which fails
  closed when a pin cannot be loaded.
- Secret values in a pinned config are rendered once, at pin load, and stay
  until the pin is evicted (same as the live config, which re-renders only on
  a new revision).
- **Claim-time owner render errors across a workspace boundary are now
  withheld** (§ 7.2), extending the dispatch policy. Scrubbing alone cannot
  match every encoding a filter chain produces, and refs make reachable
  secrets that exist only at an unreviewed commit.
- **Sync webhook output is redacted** with the per-job set, failing closed
  (§ 7.5). Today it is returned unredacted for every job.

## 10. Retention, limits, configuration

**Keep-set** (replica-local, evaluated on every replica's tick — not
leader-gated, because the store is local):

- `(job.workspace, job.revision)` of active jobs with `job.ref` set;
- `(action_workspace, action_revision)` of active jobs' steps with
  `action_ref`; `(task_workspace, task_revision)` with `task_ref`;
- failed pinned top-level jobs still owed a task retry (the 1-hour window of
  `tarball_keep_revisions`, `stroem-db/src/repos/job.rs:618`);
- plus the `keep_recent_per_workspace` most recently used pins.

Eviction drops the in-memory config and the checkout dir, only for entries no caller still holds (§ 5.3 leases). The bare repo is
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
- Worker: sends `?job_id=` on state and global-state downloads (§ 7.6). No
  other change: `ClaimResponse` gains no field.

## 12. Security — accepted risk

There is no gate (D7). Anyone who can merge a `ref:` into **any** workspace
can make the server run **any** branch, tag or commit of **any** configured
git workspace with that workspace's secrets — including making the server
resolve whatever `vals` references an unreviewed branch writes. Mitigation
(follow-up): an allow-list per owner workspace, plus "a SHA must be reachable
from an allowed ref".

## 13. Rollout

- Migration 049 adds columns only (instant), and old code ignores them.
  Migration 050's index build locks `job` writes for one table scan; for a
  zero-downtime rollout, pre-run it `CONCURRENTLY` as described in § 6.
- Workers should ship with the server. Uploads are always partitioned
  correctly, because the server derives the partition from the job. An old
  worker's **download** carries no `job_id` and mounts the `NULL` partition
  into a pinned job's `/state`. Template rendering is server-side and stays
  correct. This is documented, and harmless while state is unused.
- `fail_or_retry` gains the optional `expected_claim` guard (§ 7.2). An old
  replica's recovery sweep does not pass it, so during a rolling deploy an
  old leader can still fail a step that a new replica just released. The
  window is one sweep interval, and it only matters once YAML uses `ref:`,
  which the rule below already defers.
- **Do not merge YAML that uses `ref:` until every server replica runs the
  release.** An old replica drops the unknown key and runs the default branch
  silently, and treats an own-workspace `action_workspace` as a live
  cross-workspace owner. Stated in the release notes and the guide.
- Behaviour changes that apply without any `ref:` in YAML, for the release
  notes:
  - a cross-workspace step's task state now uses the job's own workspace
    (§ 7.6);
  - owner-side claim render errors of cross-workspace actions are withheld
    (§ 7.2);
  - sync webhook output is redacted (§ 7.5);
  - a webhook is re-matched and re-authenticated after `force_refresh`
    (§ 7.5);
  - an old revision, or an erroring git workspace's pinned revision, is
    served instead of 404 (§ 5.4).

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
  `retry_at` and a fresh `ready_at`; release cap → failed.
- `release_claim` racing cancellation, in both orders. Cancel first: the
  step is settled `cancelled`, its `ready` siblings are cancelled in the
  same transaction, then terminal handling runs (a sibling left `ready` would
  fail the test). Release first: `cancel_pending_steps` cancels the
  now-`ready` step.
- `release_claim` at the cap returns `CapReached` without writing, and the
  following `fail_claimed_step` is `NotApplied` if the claim changed.
- Withholding at claim: a foreign owner's action default with a failing
  filter chain over a ref-only secret yields the fixed sentence in
  `error_message`, the job log and the 422 body, and the scrubbed chain in
  the server log. A caller-side `input:` error and an own-workspace ref error
  stay visible (scrubbed).
- Sync webhook (both branches) redacts a ref-only secret in `output`, and
  answers 503 with `job_id` when the pin is unavailable.
- Job-scoped ACL inventory: for a pinned job in a denied `task_folder`
  whose live namesake sits in an allowed folder, every path in § 7.8's table
  denies. That covers job detail, REST logs, artifacts, WebSocket (backfill
  and live), worker detail rows and MCP status/logs; lists and counts are
  covered above.
- Worker detail redacts a ref-only secret in a step's `error_message`, and
  masks the field (row-level fail-closed) when that job's pin is
  unavailable.
- Webhook job-status poll (every branch) masks a ref-only secret in
  `output`.
- Job detail masks a ref-only secret inside the copied `approval_message`.
- Task stats exclude pinned jobs from the aggregates, the recent durations
  and the per-step breakdown.
- Webhook with `force_refresh`: a refresh that changes the `ref` runs the
  new ref; one that removes the webhook gives 404; one that rotates the
  secret authenticates against the new secret.
- A skipped scheduler fire for `T@ref` is recorded in `T` with `job.ref`,
  the commit and `task_folder`.
- Re-run and restart of a pinned trigger job whose task exists only at the
  ref: the ref is re-resolved, and restart's plan uses the pinned flow.
- ACL: a pinned job's folder is its `task_folder` even when a task of the
  same name exists live under another folder; an unpinned job uses the live
  folder. A release-only task's jobs are listed for a user allowed by its
  `task_folder` and hidden for one who is not. `check_job_acl` and MCP
  agree.
- A hook whose action is a ref'd `type: task` action is not fired and logs
  to the source job.
- Cross-workspace step state: upload and download (with `job_id`) use the
  job's `(workspace, task, ref)`. A download without `job_id` keeps today's
  path coordinates.
- `download_workspace` serves a pinned revision of an erroring git workspace
  (health gate bypassed for git revisions), and keeps the live path for the
  healthy current revision.
- A recovery failure carrying a stale `expected_claim` → `NotApplied` on a
  released step and on a step reclaimed by another worker.
- Inheritance: a local `type: task` in a pinned job stamps `task_*`. A
  pinned job calling a live foreign `B.run` with an unqualified task stays
  unpinned. Dispatch reads only `task_*`.
- Prefix-strip invariant: own-workspace ref (undotted), `owner.x` + ref
  (stripped to `x`), and an inherited library action `common.x` in a pinned
  job (full-key lookup).
- A ref'd agent action → 400; an agent step inside a pinned job claims
  against the pinned config.
- A cross-workspace webhook / scheduler target with an approval root step:
  the initial `on_suspended` hooks come from the target's (pinned) config.
- `task_state.ref` isolation (two refs, same task), NULL-safe lookup, the
  upload partition taken from `job.ref` (a client cannot choose it),
  download with and without `?job_id=`.
- Redaction covers a secret value that differs between `main` and the pin;
  job detail answers 503 when a referenced pin is unavailable.
- Claim connection resolution for a pinned foreign owner uses the pinned
  owner's connections (overlay), not the live ones.
- `resolve` race: the branch moves between ls-remote and fetch, and the
  fetched tip is adopted. A stamped SHA missing on a cold store is fetched
  by SHA.
- `pin_dir` lock: a second store on the same dir fails to start. Eviction
  skips an entry whose handle is still held.
- Role-scoped lookup: pinned job `W@X` calling `action: import, ref: R2`,
  where a connection `db` differs between `X` and `R2`. The caller-supplied
  `db` resolves at `X`, the action's default `db` resolves at `R2`, and a
  release-only connection named by the caller falls back to `R2` ungated.
  Covered at the pre-check, at claim and at dispatch.
- ACL lists with two refs of one release-only task under different folders:
  a user allowed only one folder sees only that ref's jobs. REST list, count,
  status counts and MCP `list_jobs` all agree. A pinned job of a task that
  also exists live uses its own `task_folder`.
- Old ordinary revision tarball served instead of 404.
- `classify_execute_error` 400/500 for each § 8 row.
- `migration_test.rs`: 049 columns, 050 indexes (new names present, old names gone); both re-runnable after a manual pre-run (idempotent).

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
- `ref` on hooks (including a hook calling a ref'd `type: task` action),
  event sources, agent task tools (§ 4.6).
- `ref` on agent actions: needs agent MCP definitions and task tools built
  from the action owner's (pinned) config. This is the same deferred work as
  cross-workspace agent actions.
- Ref-partitioned manual state upload (§ 7.6).
- Pinning definitions for **unpinned** jobs (today's live-config drift).
- Libraries at a ref; bare-repo `git gc`; sharing pins across replicas.
- Pre-existing, found during design; recorded in TODO.md and not made worse
  here:
  - the tarball-cache cleanup runs on the leader only (`recovery.rs:441`),
    while the cache is replica-local;
  - `fail_or_retry` with an empty expected list accepts any status, so a
    recovery sweep can race a worker's completion. This design guards only
    the new release path and `fail_claimed_step` (§ 7.2);
  - job **logs** are never value-redacted on read: REST, WebSocket and MCP
    `get_job_logs` all return raw lines. A script that prints a secret
    leaks it whether or not refs are involved;
  - cancellation is two non-atomic calls (`JobRepo::cancel`, then
    `cancel_pending_steps`), and the claim SQL does not check the job's
    status (`job_step.rs:561`). A worker can claim a `ready` sibling between
    the two calls. `release_claim` does not widen this: it cancels siblings
    itself when it finds the job terminal.
