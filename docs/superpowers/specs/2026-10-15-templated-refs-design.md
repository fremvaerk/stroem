# Templated refs (`ref: "{{ … }}"`)

Status: draft, revision 5 (2026-10-15)
Builds on: `2026-10-02-git-refs-design.md` (the git-refs spec, cited as GR),
`2026-10-06-tera-2-upgrade-design.md` (cited as T2),
`2026-09-11-render-context-owner-design.md`.

## 1. Goal

Let the person who runs a task, the system that calls a webhook, or an earlier
step choose which branch, tag or commit an action runs from:

```yaml
tasks:
  deploy:
    input:
      branch: { type: string, default: main }
    flow:
      build:
        action: build
        ref: "{{ input.branch }}"          # chosen in the Run form
      find:
        action: find-release
      publish:
        action: billing.publish
        ref: "{{ find.output.branch }}"    # chosen by an earlier step
        depends_on: [find]

triggers:
  on-push:
    type: webhook
    task: build
    ref: "{{ input.body.ref }}"            # chosen by the caller
```

Today a `ref:` is a static string (GR D1); `parse_git_ref` rejects any value
containing `{{` or `{%` (`crates/stroem-common/src/git_ref.rs:55`).

**Success criteria.**

1. A flow-step `ref:` can read the job input, the outputs of earlier steps,
   `state`, `global_state`, `job`, and `each` (for a `for_each` instance).
2. A `type: task` action's `ref:` can read the caller's step input, rendered
   as strings (§ 5.6).
3. A webhook trigger's `ref:` can read the webhook job input
   (`body`, `headers`, `method`, `query` and the trigger's `input` defaults).
4. Inside one job, every step that uses the same `(owner, ref)` runs the same
   commit — static and templated refs alike — even if the branch moves.
   Exception: a Restart From Step job, whose carried rows keep the commits
   of the source job while the steps that run again resolve fresh (§ 5.7) —
   the same exception static refs already have.
5. Static refs, and jobs without `ref:`, behave exactly as today.

## 2. Background

- **Resolution happens at job creation.** `create_job_for_task_inner`
  (`crates/stroem-server/src/job_creator.rs:414`) resolves every flow-step ref
  through `resolve_step_action` (`job_creator.rs:1291`) and a per-creation
  `RefMemo` (`job_creator.rs:1089-1116`), then builds every step row with
  `build_step` (`job_creator.rs:872`) and inserts them all at once
  (`crates/stroem-db/src/repos/job_step.rs:347`). The row carries everything
  derived from the action definition: `action_type`, `action_spec`,
  `action_image`, `runner`, `required_ability`, `required_tags`, the retry
  columns, `action_workspace`, `action_revision`, `action_ref` and the
  `task_*` columns.
- **Worker selection happens in SQL.** The claim
  (`job_step.rs:640-670`) matches the claim request's capabilities and tags
  (`$1::jsonb @> to_jsonb(required_ability)`, `required_tags::jsonb <@ $2`,
  `job_step.rs:657-658`) before any Rust code sees the step. The
  partial index `idx_job_step_ready_claim`
  (`crates/stroem-db/migrations/044_claim_index_agent.sql:9`) and the
  unmatched-step sweep (`job_step.rs:1335`) carry the same
  `action_type NOT IN ('task', 'approval', 'event_source')` predicate.
- **Abilities are a closed set at registration only.** `/worker/register`
  rejects any capability not in
  `KNOWN_ABILITIES = ["script", "docker", "kubernetes", "agent"]`
  (`crates/stroem-server/src/web/worker_api/jobs.rs:272`, check at `:339`).
  The claim SQL, however, matches the capabilities sent in the CLAIM request
  (`$1::jsonb @> to_jsonb(required_ability)`, `job_step.rs:657`), and
  `claim_job` (`worker_api/jobs.rs:819-858`) rejects only an empty list — it
  never checks the values.
- **Server-dispatched kinds.** `type: task` and `type: approval` steps are
  promoted to `ready` like any step; the claim excludes them and
  `Settlement::advance` dispatches them after the cascade
  (`crates/stroem-server/src/settlement/mod.rs:295`: cascade `:321`, task
  dispatch `:324`, reconcile `:343`, approval dispatch `:474`).
- **`for_each` instances copy the placeholder.** R4 copies `action_type`,
  `action_spec`, `required_ability` and the pin columns from the placeholder
  into each instance (`crates/stroem-server/src/cascade.rs:536-559`).
- **Tera 2 keeps its parser private** (`mod parsing;`, `tera-2.4.0/src/lib.rs:77`),
  so Strøm cannot list the variables a template reads. The `default` filter
  accepts an undefined value (`tera-2.4.0/src/filters.rs:95`), so a test render
  with a reduced context does not tell whether a template depends on the
  missing variables.
- **Identifiers skip redaction.** `action_ref`, `task_ref` and the job's `ref`
  are in `STEP_IDENTIFIER_KEYS` / `JOB_IDENTIFIER_KEYS`
  (`crates/stroem-server/src/redaction.rs:127`, `:155`), so job-detail
  redaction never masks them.
- **Input `options` are a UI hint.** The server checks defaults against
  `options` (warnings, `crates/stroem-common/src/validation.rs:1043-1090`) but
  never a submitted value. They are not a security boundary.

## 3. Decisions

| # | Decision | Chosen | Rejected alternatives |
|---|---|---|---|
| D1 | Who picks the ref | A template in `ref:` — job input, step outputs, `each`, webhook request | Per-run ref override (still a follow-up, GR § 16) |
| D2 | When a templated ref resolves | Flow step: at promotion (`pending → ready`). `type: task` action: at dispatch. Webhook trigger: when the webhook fires | At claim (worker selection needs the action first, § 2); at creation only (no step outputs yet) |
| D3 | Where a template is allowed | Flow-step `ref:`, `type: task` action `ref:`, webhook trigger `ref:` | Scheduler trigger (static input only); hooks, event sources, agent task tools (`ref:` itself unsupported, GR § 4.6) |
| D4 | Trust gate | **None** — extends GR D7 (§ 10.1) | Required / optional `ref_allow` glob list next to the ref; owner-workspace policy |
| D5 | One commit per ref per job | A persisted memo, table `job_ref_pin`, seeded at creation, first writer wins | Per-step commits (a moving branch splits the job) |
| D6 | Empty rendered value | Error | Treat as "no ref" (follow-up, § 14); the git source's default branch (surprising inside a pinned job) |
| D7 | Temporary git failure while resolving at promotion | Fail the step (like `type: task` dispatch) | Keep `ready`, retry with a cap, recovery re-advances |
| D8 | Creation-time pre-check of templated refs | **None** | Syntax rule (`{{ input.x }}` resolves at creation); a template-text scanner; a reduced-context test render (unsound: `default` hides undefined variables, § 2) |
| D9 | How a deferred step stays unclaimable | `action_type = 'deferred'` **and** `required_ability = 'deferred'`; claim SQL and index unchanged; the claim handler rejects a capability outside `KNOWN_ABILITIES`, as register does | Add `'deferred'` to the claim `NOT IN` list (needs a new partial index on `job_step`) |
| D10 | Secrets | No context contains `secret`; a rendered ref that contains a value of the job's *ref secret set* (§ 10.3) is rejected | Masking the identifier columns; the full redaction closure as the check set (a closure walk per step, and a `MaskAll` outcome) |
| D11 | Owner of a templated flow-step ref | Decided from the action name by `refs::ref_owner`, at creation; the template selects only the commit | A template that also selects the workspace |
| D12 | One resolver per deferred step | A short renewable lease with an owner token (`job_step.resolve_lease_owner`, `resolve_lease_until`); both final writes fence on the token | A recovery threshold only, or a fixed-length lease (a resolve can wait on the PinStore's resolve lock with no acquire timeout, so no fixed length is safe); unguarded concurrent resolution |
| D13 | Ref template language | Full Tera, minus `secret` and `vals` (§ 4.2) | A restricted grammar — literal text, variable paths and a literal `default` — that would close the transformed-secret path by construction (§ 10.3). Declined by the user; the risk is accepted (§ 10.1) |

## 4. YAML surface and semantics

### 4.1 Where a template is allowed

| Place | Renders and resolves | Context (render-context insertion order) |
|---|---|---|
| Flow-step `ref:` | When the step is promoted to `ready` (§ 5.3) | `input` (job input), `state`, `global_state`, `job`, completed step entries, `each` (instances only) |
| `type: task` action `ref:` | At dispatch, after the step is `ready` (§ 5.6) | `input` (the caller's step `input:` rendered as strings, with no schema and no action defaults), `job` |
| Webhook trigger `ref:` | When the webhook fires, before job creation (§ 6) | `input` (the webhook job input) |

A scheduler trigger `ref:` stays static: a template is a validation error.
Hooks, event sources and agent task tools still reject every `ref:`
(GR § 4.6, unchanged).

### 4.2 Rules

1. **Classification.** A `ref:` is a template if it contains `{{` or `{%`
   (new `stroem_common::git_ref::classify_ref(&str) -> RefKind::{Static, Template}`).
   A static `ref:` is parsed and resolved exactly as today.
2. **No secrets in the context** (D10). Two new `render_context::Scope`
   variants, built by `render_context::build`
   (`crates/stroem-server/src/render_context.rs:380`) like every other scope,
   with no patching after `build`: `Scope::StepRef` (job input; `each` comes
   through `build`'s existing `loop_slot` parameter, set only for a
   `for_each` instance) and `Scope::TaskActionRef { ref_input: &Value }`.
   Neither inserts `secret`. The webhook ref is rendered before any job
   exists, so `build` (which takes a `JobContext`) does not fit; a separate
   constructor in the same module, `render_context::webhook_ref(input: &Value) -> RenderContext`,
   builds a context holding only `input`. `RenderContext` stays opaque and is
   constructed only in `render_context.rs`.
   No ref render can call `vals` either: `vals` fetches secret values from
   external stores, and a ref is an identifier. Validation rejects the `vals`
   filter in a `ref:` template, and the ref template is rendered in a mode in
   which `vals` is not registered, so a use is a render error. (The § 5.6 ref
   input is a step-input render, not a ref render: it keeps today's
   step-input context, `secret` and `vals` included.)
3. **Post-render check.** The rendered value is trimmed, then:
   - empty → error (D6);
   - contains a known secret value → error (§ 10.3);
   - `parse_git_ref` → a branch, a tag or a full 40-hex SHA, exactly as a
     static ref (a template marker in the RENDERED value is also rejected, so
     a value cannot render into another template).
4. **Owner** (D11). The owner of a templated flow-step ref is decided at
   creation from the action name by `refs::ref_owner`: undotted → the job's
   workspace, `lib.x` → 400, a configured `ws.x` → `ws`, otherwise unknown
   workspace (400). The owner must be a git workspace (400 otherwise, as for a
   static ref). Only the commit waits for promotion.
5. **What a template cannot change.** The step's `depends_on`, `when` and
   `for_each` come from the job's config (live or pinned), never from the
   ref'd commit, exactly as for a static ref.

## 5. Lifecycle of a deferred step

### 5.1 At job creation

For a flow step whose `ref:` is a template, `create_job_for_task_inner`
does not call `resolve_step_action`. It builds a **deferred row**:

| Column | Value |
|---|---|
| `action_type` | `'deferred'` |
| `required_ability` | `'deferred'` |
| `action_workspace` | the owner (§ 4.2 rule 4) |
| `action_name` | the action's name in the owner (the part after `ws.`, or the whole name) |
| `ref_template` (new) | the template as written |
| `runner` | `'none'` (the column is `NOT NULL` with the check `local, docker, pod, none`, `004_tags_runner.sql:7`, `:22-23`) |
| `action_spec`, `action_image`, `action_revision`, `action_ref`, `task_*` | `NULL` |
| `resolve_lease_owner`, `resolve_lease_until` (new) | `NULL` |
| `required_tags` | `[]` |
| `input`, `timeout_secs`, `when_condition`, `for_each_expr` | from the flow step, as today |
| retry columns | from the flow step's `retry:` when present; otherwise filled at resolution |

The template is stored on the row, not read again from the config at
promotion, because the live config can change during the job.

Creation-time checks that need the action move to resolution
(§ 5.3 step 6): `precheck_literal_connection_inputs`
(`job_creator.rs:1415`) and the `type: agent` rejection
(`RefResolveError::AgentAction`).

### 5.2 The ref memo (`job_ref_pin`)

```sql
CREATE TABLE job_ref_pin (
    job_id    UUID NOT NULL REFERENCES job(job_id) ON DELETE CASCADE,
    workspace TEXT NOT NULL,
    git_ref   TEXT NOT NULL,   -- the ref as written / as rendered
    commit    TEXT NOT NULL,
    PRIMARY KEY (job_id, workspace, git_ref)
);
```

- At creation, inside the creation transaction, every `RefMemo` entry is
  written: the job's own pin and every static ref the creation resolved.
- At resolution: read `(job_id, owner, rendered)`. On a miss, the pass
  resolves through the PinStore and loads and checks the selected commit
  (§ 5.3 steps 3–4) BEFORE it writes the memo: `INSERT … ON CONFLICT DO NOTHING`,
  then read again. If the stored commit differs (another resolver wrote
  first), the pass loads THAT commit and repeats the check. The first writer
  decides the commit for the rest of the job. A value that fails a check is
  never written to the memo.
- Keyed by the string as written, like `RefMemo` (`release/2.3` and
  `refs/heads/release/2.3` are two keys).
- A child job (`type: task`) has its own memo, seeded at its creation, as
  today's `RefMemo` is per creation.
- `JobRepo::pin_keep_set` (`crates/stroem-db/src/repos/job.rs:848`) also keeps
  the `job_ref_pin` commits of non-terminal jobs, so a resolved commit stays
  in the PinStore until its step is claimed.

### 5.3 When the step is `ready`: `resolve_deferred_steps`

A deferred step reaches `ready` in two ways, and neither changes:

- a **root** step with no `depends_on`, no `when` and no `for_each` is
  inserted `ready` at creation (status chosen at `job_creator.rs:693-697`);
- every other step — a dependent step, or a root step with `when` or
  `for_each` (inserted `pending`) — is promoted by the cascade like any step:
  the creation-time cascade in `init` for roots, a later cascade for
  dependents. Its `when` is evaluated first; a skipped step is never resolved.

So the new pass `resolve_deferred_steps` runs in BOTH places that dispatch
server-side steps today, after the cascade and before task dispatch:

```
Settlement::advance step 1:  cascade_and_settle → resolve_deferred_steps → task dispatch → reconcile → approval dispatch
dispatch::init (creation):   cascade::execute   → resolve_deferred_steps → task dispatch → approval dispatch
```

(`settlement/mod.rs:321-474` and `settlement/dispatch.rs:1001-1047`.) Without
the `init` call, the headline case — a root step with
`ref: "{{ input.branch }}"` — would wait for recovery (§ 5.8).

Consequences of the `init` call:

- A resolution failure of a root step is a per-step failure
  (`fail_deferred_tx` + cascade, § 5.5), exactly like a root `type: task` dispatch failure inside
  `handle_task_steps`. It is NOT the coordinated
  `[creation] initialisation failed` path; only an error that escapes the pass
  (a DB error) reaches that path.
- With no pre-check (D8), an HTTP execute whose root templated ref names a
  wrong branch returns the created job (200); its root step is already failed.
- An agent task-tool child whose only root step fails resolution at `init` is
  born terminal: `agent_child_created` returns `Err(BornTerminal)` → 500 to
  the worker, as for a server-dispatched root step that fails today.
- `init` runs inside the creating request, so a cold pin resolve lengthens that
  request — the same cost a static ref has today.

For each `ready` step of the job with `action_type = 'deferred'`:

0. **Take the lease** (D12). The resolver makes a fresh token
   (`Uuid::new_v4()`) and runs
   `UPDATE job_step SET resolve_lease_owner = $token, resolve_lease_until = NOW() + interval '60 seconds' WHERE job_id = $1 AND step_name = $2 AND status = 'ready' AND action_type = 'deferred' AND (resolve_lease_until IS NULL OR resolve_lease_until < NOW())`.
   Zero rows → another resolver holds a live lease, or the step has moved on;
   skip the step. While steps 1–7 run, a renewal task extends the lease every
   20 s (`SET resolve_lease_until = NOW() + interval '60 seconds' WHERE … AND resolve_lease_owner = $token AND status = 'ready' AND action_type = 'deferred'`);
   it stops when the resolution ends. A renewal that changes no row means the
   lease is lost: the resolver stops before it writes. The lease is never
   released explicitly — on success the step is no longer deferred, on
   failure it is failed. Constants `DEFERRED_LEASE_SECS = 60`,
   `DEFERRED_LEASE_RENEW_SECS = 20`. A resolver that waits a long time (the
   PinStore's resolve lock has no acquire timeout, `pins.rs:787-788`; a cold
   load can take several `load_timeout_secs`) keeps its lease by renewing; a
   resolver whose process dies loses it within 60 s.
1. Build the context with `render_context::build(job, steps, loop_slot, Scope::StepRef)`
   from the same `Snapshots` the advance already holds (`loop_slot` set for a
   `for_each` instance).
2. Render `ref_template`; apply the post-render check (§ 4.2 rule 3).
3. Look up `(job_id, owner, rendered)` in `job_ref_pin`; on a miss, resolve
   `owner@rendered` with `PinStore::resolve` (no memo write yet). Until the
   value passes the check in step 4 it is private: it reaches no span, log
   line or error text (§ 5.5).
4. Load the owner's config at the commit through the private pin-load path
   (§ 5.5), then check the rendered value against that commit's own secrets
   (`pin_redaction_values` of the selected pin, § 10.3). Only then write the
   memo (§ 5.2); if the stored commit differs, repeat this step with it. A
   failed check fails the step and writes no memo row.
5. Find the action and build the row values with the SAME code creation
   uses. `resolve_step_action` and the per-step part of
   `create_job_for_task_inner` are refactored into one function,
   `job_creator::resolve_step_row(…, git_ref_override: Option<&str>) -> NewJobStep`,
   called by creation (static refs) and by this pass (rendered refs). For a
   `type: task` action it calls `resolve_task_for_step` as creation does.
6. Run the moved creation checks: literal connection inputs against the
   resolved action's schema; `type: agent` is rejected.
7. Write the real columns in one transaction, `JobStepRepo::resolve_deferred_tx`:
   first `SELECT status FROM job WHERE job_id = $1 FOR SHARE` — the lock order
   of `JobStepRepo::release_claim` (`job_step.rs:934-960`), which serialises
   with `JobRepo::cancel`'s write of the job row; if the job is not `pending`
   or `running`, write nothing; otherwise
   `UPDATE job_step SET … WHERE job_id = $1 AND step_name = $2 AND status = 'ready' AND action_type = 'deferred' AND resolve_lease_owner = $token`.
   Zero rows → the step was cancelled or timed out, or the lease was lost to
   another resolver; the pass moves on. The token fence makes a lost lease
   harmless: only the current holder can write. With the
   job row held `FOR SHARE`, `JobRepo::cancel` either commits first (the pass
   sees a terminal job and writes nothing) or waits until the resolution
   commits (and `cancel_pending_steps` then cancels the resolved `ready` row,
   § 5.7).

After the pass, a step whose real type is `task` or `approval` is picked up
by the dispatch passes that follow it in the same `advance` / `init`; a worker
type is claimable.

The pass loops like `handle_task_steps`: a failure (§ 5.5) re-cascades, which
can promote further deferred steps; the loop repeats while it makes progress,
bounded by the number of steps in the job.

**I/O inside `advance`.** Steps 3–4 can do an ls-remote, a fetch, a checkout
and a config load, each bounded by `workspace_reload.load_timeout_secs`; the
PinStore's resolve lock has no acquire timeout (known gap, CLAUDE.md § Health
Check, TODO.md). `type: task` dispatch already loads pinned configs inside
`advance` (GR § 7.3), so this adds no new class of blocking. The cascade's
`run` stays pure: resolution never happens inside it.

### 5.4 `for_each`

The placeholder of a deferred step stays deferred for its whole life — it never
runs, and R6 rolls it up from its instances as today. R4 copies the deferred
columns, including `ref_template`, into each instance (`cascade.rs:536-559`
gains `ref_template`). Each instance is resolved by the pass with `each` in
its context. Instances that render the same value share one commit through
`job_ref_pin`. Sequential instances are resolved when R5 promotes them.

### 5.5 Failure

- Every resolution error — render error, empty value, secret in the value,
  invalid name, ref not found, commit not found, a non-git owner, a missing
  action, `type: agent`, a failed connection pre-check, and a temporary
  `PinUnavailable` (D7) — fails the step through a NEW guarded write,
  `JobStepRepo::fail_deferred_tx`, in the same transaction shape as § 5.3
  step 7: the job row `FOR SHARE`, nothing written unless the job is `pending`
  or `running`, then
  `UPDATE job_step SET status = 'failed', error_message = $3, completed_at = NOW() WHERE job_id = $1 AND step_name = $2 AND status = 'ready' AND action_type = 'deferred' AND resolve_lease_owner = $4`,
  returning whether it changed a row. A failure that loses to a cancellation
  writes nothing, so the step ends `cancelled`, not `failed`. Plain `mark_failed`
  (`crates/stroem-db/src/repos/job_step.rs:781-790`) is NOT used: it matches
  on job and step name alone and would overwrite a row a concurrent advance
  has already resolved, a worker has claimed, or a step that has completed.
  Only a write that changed a row is followed by `cascade_and_settle` (the
  `fail_task_step` pattern, `settlement/dispatch.rs:88`). The failure does not
  consume a retry attempt and does not go through `fail_or_retry`: the step
  has not run.
- Messages are value-free (T2 § 3.2 and the CLAUDE.md post-render rule): they
  name the step, the owner workspace and the error kind, never the rendered
  value. Example: `[ref] step 'publish': the rendered ref was not found in workspace 'billing'`.
  Template errors are already value-free (`TemplateError`).
- **A rendered value is private until it passes both secret checks**
  (§ 10.3). After that it is an identifier, like a static ref, and appears
  wherever `action_ref` / `task_ref` / the job's `ref` appears — job detail,
  and spans such as `create_job_for_task_pinned`'s `git_ref` field
  (`job_creator.rs:343`). Before that, no span, log line or error text may
  carry it. Today's pin code does not meet this:
  - `PinStore::resolve` records `git_ref` as a span field
    (`crates/stroem-server/src/workspace/pins.rs:761`);
  - `PinError`'s `Display` prints it (`RefNotFound`, `pins.rs:317`;
    `PinUnavailable`'s text, `pins.rs:799-804`);
  - `pin_label` names a pin `ws@ref (sha)` (`pins.rs:345-347`), and
    `WorkspaceManager::pin_error_for_user` logs that label with the loader
    chain on a `PinLoadFailed` (`workspace/mod.rs:570-585`);
  - the webhook handler logs and classifies errors by their text
    (`web/hooks.rs:102`, `web/api/mod.rs:424`).

  Changes:
  1. `PinStore::resolve` stops recording `git_ref` in its span, for every
     caller (`workspace` stays). No caller reads that field; for static refs
     this only changes what the server log shows.
  2. **A private pin-load path.** The pass, § 5.6 and the webhook load the
     selected config with the pin marked private (for example a
     `PinDisplay::CommitOnly` flag on `PinRef`). For such a pin, `pin_label`
     prints `ws (sha)`, and `pin_error_for_user` logs the scrubbed chain under
     that commit-only label. Static refs keep `ws@ref (sha)`.
  3. **One typed error leaves the new paths.** They classify every resolution
     error with `downcast_ref` (`PinError`, `GitRefError`, `PinLoadWithheld`,
     `RefResolveError`, `TemplateError`) and turn it into a new
     `RenderedRefError { kind }`, whose `Display` is a fixed sentence per kind
     (`Empty`, `Invalid`, `SecretValue`, `NotFound`, `NotGit`, `NoAction`,
     `AgentAction`, `LoadWithheld`, `Unavailable`, `Render`, `Unresolvable` for
     an untyped error). It is the only error that leaves a resolution
     function, so the job log, `error_message`, `tracing`, the webhook's error
     log and `classify_execute_error` see only that text.
     `classify_execute_error` gets a branch for it: `Unavailable` → 500, every
     other kind → 400. Static-ref paths keep their current messages.

### 5.6 A `type: task` action with a templated `ref:`

The step is an ordinary `type: task` step. At creation `resolve_task_for_step`
is not called for it (today it would fail with `GitRefError::Templated`); its
`task_*` columns stay `NULL`. `action_spec` carries the whole action
definition — `build_step` stores `serde_json::to_value(action)`
(`job_creator.rs:886`), and `ActionDef.git_ref` serializes as `ref` when set
(`crates/stroem-common/src/models/workflow.rs:265-266`) — so the template is
on the row.

At dispatch, `handle_task_steps_pass`
(`settlement/dispatch.rs:188`) classifies `action_spec.ref`. Today dispatch
resolves the task first and only then renders the step input, because the
TASK's schema decides how each field renders (a `json` field yields a native
value, other fields strings — `render_input_typed`,
`crates/stroem-common/src/template.rs:537`; dispatch selects `T`'s config at
`settlement/dispatch.rs:336` and renders with it from `:339`). A templated
task ref would make that circular, so the ref gets its own, schema-free input.
The order is a constraint:

1. **ref input**: a recursive, schema-free projection of the caller's step
   `input:` map (new `template::render_ref_projection`): every string leaf,
   at any depth, is rendered as a string; other leaves are copied; arrays
   keep their length and order. Its context is exactly the one the real
   step-input render uses (`Scope::ChildTaskInput`: job input, caller
   secrets, `vals` available), so a field that works in the child input also
   works here. ANY leaf error fails the step through `fail_task_step` with a
   value-free message — nothing is left out, so a `default` in the ref can
   never pick a fallback because a leaf silently failed. It is a caller-side
   error (bucket `C`, never withheld). It is not `render_input_map`, which
   renders top-level strings only (`template.rs:484-495`). The projection is
   used ONLY to build the ref context; it is never persisted, logged or passed
   to the child;
2. render the ref with `Scope::TaskActionRef { ref_input }` (no `secret`) and
   apply the post-render check;
3. resolve the task owner `T` syntactically from `action_spec.task` relative
   to the action owner `O` (as `refs::ref_owner` does for a static ref); look
   up `T@rendered` in the parent job's `job_ref_pin` or resolve it; load
   `T`'s config at that commit through the private path (§ 5.5); check the
   value against `T`'s pin secrets (§ 10.3); only then write the memo (§ 5.2);
4. find the task in that config;
5. from here the existing path runs unchanged: render the step input with
   `T`'s schema, merge the action defaults (bucket `D`, rendered once — spec
   R26), `resolve_task_input_by_provenance(_roles)`, stamp `task_*` on the
   parent step, create the child pinned.

Consequences: the caller's step input is rendered twice — every `vals`
lookup in it runs twice, and a value that can change between two renders
(`now()`, a `vals` lookup) can give the ref a different value than the child
input; the ref uses the first render. The projection can hold values derived
from caller secrets; the ref template itself has no `secret` and no `vals`,
and a verbatim secret in its output is rejected, but a filter can transform
one (§ 10.1, accepted).
Action defaults are not visible to the ref — an action that wants a default
branch writes `{{ input.branch | default(value="main") }}`. This also keeps
the owner's secrets, which bucket `D` renders with, out of the ref context.
The ref-input render's errors are caller-side (bucket `C`, never withheld)
and go through `fail_task_step`, value-free.

Failures go through `fail_task_step`, as every dispatch failure does today;
its unguarded `mark_failed` is part of the known double-dispatch gap (§ 14),
which this spec neither fixes nor widens. The ref template is written by the action owner `O`, so
its render error is withheld when `O != A` (owner-side origin, CLAUDE.md
§ Cross-Workspace References); a post-render error is value-free in every
case.

A flow step with a templated `ref:` whose resolved action is a `type: task`
action with its own templated `ref:` combines the two: § 5.3 resolves the
action, § 5.6 resolves the task at dispatch.

### 5.7 What does not change

- **The claim.** A resolved row is like any other row; `ClaimResponse` gains no
  field.
- **Step retry** (`fail_or_retry`): the row is already resolved, so every
  attempt runs the same commit.
- **Restart from step** — the exception to success criterion 4: carried rows
  keep their columns; steps that run again get new deferred rows and resolve
  again. The restart job's memo is seeded from its own creation only (not from
  the carried rows' pins), so a re-run step can get a newer commit than a
  carried step that used the same ref. Static refs behave the same way today:
  they re-resolve at restart while carried rows keep the source's commits.
- **Task retry, re-run**: a new job with a new memo; templates resolve again.
- **Cancellation, job timeout**: `Settlement::cancel` writes the job row
  (`JobRepo::cancel`) and then, in a separate call, cancels the job's
  `pending`/`ready`/`suspended` steps (`JobStepRepo::cancel_pending_steps`,
  `job_step.rs:1172`; `settlement/mod.rs:799-816`). A resolution that runs
  after the job row is cancelled writes nothing (job-status condition, § 5.3
  step 7). A resolution that commits just before it leaves a resolved `ready`
  row, which `cancel_pending_steps` then cancels like any `ready` step; a
  worker that claims it in between is stopped by the cancellation signal.
  That last window is the one every `ready` step has today (the claim SQL
  does not read the job's status) and is not changed here (TODO.md).
- **Step timeout** is measured from `started_at` (claim), so resolution time
  does not count. Job timeout does.

### 5.8 Recovery

A crash — or an advance that dies — after the cascade or creation commit and
before the pass writes leaves a `ready` deferred row that no worker can claim
and no sweep revisits: the unmatched-step sweep excludes it (§ 8), and Phase
4.5 skips jobs that have a `ready` step (`job.rs:1395-1398`).

New recovery phase 4.6, `readvance_deferred_steps` (leader only, next to Phase
4.5, `crates/stroem-server/src/recovery.rs:338`): list non-terminal jobs that
have a step with
`status = 'ready' AND action_type = 'deferred' AND ready_at < NOW() - unmatched_step_timeout_secs AND (resolve_lease_until IS NULL OR resolve_lease_until < NOW())`
and `advance` each. Bounded and resumable like Phase 4.5
(`StalledSweepBounds`, a cursor on `AppState`, one heartbeat per job). Only a
lease holder resolves (§ 5.3 step 0), so recovery never starts a second
resolution beside a live one: a slow resolve keeps renewing its lease, and a
dead resolver's lease expires within `DEFERRED_LEASE_SECS`. Even if a lease
is lost while its holder still runs, the token fence (§ 5.3 step 7, § 5.5)
lets only the new holder write.

**Out of scope (cut after Codex round 2).** A crash between the pass's
`UPDATE` (real type `task` / `approval`) and the dispatch that follows leaves
a `ready` task / approval row. Every cascade-promoted `type: task` /
`type: approval` step has the same window today. Re-advancing such rows from
recovery is unsafe while dispatch is not idempotent: `mark_running_server` and
`mark_suspended` do not report whether they won their guarded update
(`job_step.rs:722-735`, `:1385`), so two dispatchers can both create a child
or both fire `on_suspended` hooks. Both belong to the lifecycle-hardening spec
(`2026-09-16-task-step-lifecycle-hardening-design.md`) and TODO.md (§ 14).

## 6. Webhook trigger `ref:`

The handler order is unchanged (CLAUDE.md § Git refs → Triggers): authenticate
against the cached secret, `force_refresh` if configured, re-match on one
config snapshot, re-authenticate against the fresh secret. Then:

1. Build the webhook job input (`crates/stroem-server/src/web/hooks.rs:608-645`).
2. If the trigger's `ref:` is a template, render it with the
   `render_context::webhook_ref` context (§ 4.2 rule 2) and apply the post-render check (§ 4.2 rule 3; the secret check uses the
   live redaction set).
3. Call `trigger_target::resolve_trigger_target` with the rendered value,
   through the private pin-load path (§ 5.5). Check the rendered value
   against the selected pin's secrets (§ 10.3) before the job is created.
   From here on it is exactly a static-ref webhook. Every resolution error
   leaves as a `RenderedRefError` (§ 5.5) before it reaches the handler's
   error log (`web/hooks.rs:102`) or `classify_execute_error`.

Errors go through `classify_execute_error`'s `RenderedRefError` branch: an
empty value, an invalid value, a render error, a secret value and a ref that
is not found are 400; `Unavailable` is 500.
No job is created. The job is an ordinary pinned job: `job.git_ref` holds the
rendered value, so state partitions, redaction and re-run/restart (which
re-resolve `job.git_ref`, `web/api/pinned_source.rs::resolve_source_pin`) need
no change; they never re-render the template.

The scheduler shares `trigger_target` but never sees a template (validation
rejects it, and `resolve_trigger_target` fails on one with `GitRefError::Templated`).

## 7. Validation and CLI

- `parse_git_ref` keeps its `Templated` error. Callers in the three permitted
  places call `classify_ref` first; a template goes to `check_template_syntax`
  (syntax, unknown filters / tests / functions).
- A template in a scheduler trigger, a hook, an event source, an agent task
  tool, or on an action of a type other than `task` is a validation error with
  a message naming the place.
- `stroem validate` skips the action / task lookup for a templated ref with a
  warning, like a static ref today
  (`crates/stroem-common/src/validation.rs:128`, `:216`):
  `… at a templated ref - validation skipped (resolved when the step starts)`.
- Job creation calls `check_template_syntax` on every templated flow-step ref
  (`ref_template`) and every templated `type: task` action ref it meets (no
  I/O, no render); a syntax error or an unknown filter is a 400 at creation.
  Validation is not wired into server loads, so without this a typo would
  first show as a failed step. Creation does not render or resolve a templated
  ref (D8).
- `stroem run` does not read `git_ref` today (it runs the local action) and does
  not read a templated one either. The user guide states this.

## 8. Storage — migration `051_templated_refs.sql` (additive)

- `ALTER TABLE job_step ADD COLUMN ref_template TEXT;`
- `ALTER TABLE job_step ADD COLUMN resolve_lease_owner UUID, ADD COLUMN resolve_lease_until TIMESTAMPTZ;`
  (§ 5.3 step 0).
- `CREATE TABLE job_ref_pin …` (§ 5.2).
- Widen `job_step_action_type_check` (last set by `026_event_source.sql:9-10`)
  with `'deferred'`: migration 051 drops it and adds the wider check
  `NOT VALID` (no table scan); migration `052_templated_refs_validate.sql`
  runs `VALIDATE CONSTRAINT`, which scans under a `SHARE UPDATE EXCLUSIVE`
  lock, so writes continue. They are two migrations because sqlx runs each in
  its own transaction, and a `VALIDATE` in 051's transaction would hold the
  `ACCESS EXCLUSIVE` lock of the `ADD` for the whole scan. `runner` keeps its
  `NOT NULL` and its check; deferred rows use `'none'`.
- No index on `job_step` changes (D9). The claim SQL and
  `idx_job_step_ready_claim` keep their predicate: a deferred row is in the
  index but never matches a claim, because no claim can carry the ability
  `deferred`. That needs one handler change: `claim_job` rejects (400) a
  capability outside `KNOWN_ABILITIES` after the legacy-tags migration, with
  the same message `register` uses — today it checks only for an empty list
  (§ 2). The worker always sends its registered (validated) capabilities, so
  no legitimate claim changes. The unmatched-step sweep (`get_unmatched_ready_steps`,
  `job_step.rs:1321`) keeps its `NOT IN` list unchanged and gains a separate
  `AND js.action_type <> 'deferred'` clause — an added conjunct still implies
  the index predicate. The plan includes an `EXPLAIN` check that both queries
  still use the index.
- Regression tests: `KNOWN_ABILITIES` never contains `"deferred"`; a claim
  carrying `"deferred"` (or any unknown capability) is a 400.
- The `job_step` column list (`job_step.rs:12`) and `JobStepRow` gain
  `ref_template`.

## 9. API and UI

- `GET /api/jobs/{id}` returns `ref_template` on each step. It is not an
  identifier key, so job-detail redaction treats it as content.
- A deferred step returns `action_type: "deferred"`. The UI step row shows a
  `ref: {{ … }}` label while the step is deferred, and the existing
  `@ ref · sha7` badge (`ui/src/lib/job-ref.ts`) after resolution.

## 10. Security

### 10.1 Accepted risk

GR § 12 is extended. There is no gate (D4). Besides anyone who can merge YAML,
now also:

- any user with `Run` on a task whose flow-step ref reads `input`;
- any caller of a webhook whose `ref:` reads the request — **anyone at all** if
  the webhook has no `secret`;
- any earlier step whose output a ref reads

can make the server run any branch, tag or commit of the owner workspace with
that commit's secrets and connections. Combined with push access to the owner
repository, this runs arbitrary code with the owner's secrets. The user guide
says so in a caution box.

A second accepted risk (D13, user decision 2026-10-15): **a transformed secret
can appear in a ref identifier.** `action_ref`, `task_ref` and the job's `ref`
are never masked (§ 2). The check of § 10.3 rejects a rendered ref that
contains a known secret value verbatim, but a Tera filter in the ref template
(`| upper`, `| replace(…)`, `| slice(…)`) applied to a context value that
holds a secret — a job input, an earlier step's output, or a § 5.6 ref input
rendered with caller secrets — can produce a string that reveals the secret
without containing it. Example: a secret `abc` submitted as `input.branch` is
masked in job detail, but `ref: "{{ input.branch | upper }}"` stores `ABC`.
A restricted ref grammar would have closed this by construction and was
declined. The user guide says: never pass a secret into a value that a `ref:`
template reads. Follow-ups: § 14.

### 10.2 Redaction closure

`redaction_closure_pins` (`job.rs:1177`) and `any_pinned_rows`
(`job.rs:1302`) read `action_ref` / `task_ref` at read time. A deferred step
gets them only at resolution; before that no content derived from the pin is
on the row (its error message is value-free). No change.

### 10.3 Secret values in a rendered ref

`action_ref`, `task_ref` and the job's `ref` are identifiers and skip
redaction (§ 2). A rendered ref is therefore rejected (value-free error) if it
contains any value of the **ref secret set**:

- for a step: the live set (`workspace_set::collect_redaction_values`) plus
  `pin_redaction_values` of the job's own pin (`job.git_ref`) and of every
  commit in the job's `job_ref_pin` — the commits this job's steps run under —
  checked BEFORE resolution, and then once more against the secrets of the
  commit the value selected, AFTER that commit's config is loaded and BEFORE
  anything is written (§ 5.3 step 4, § 5.6 step 3);
- for a webhook: the live set before resolution, then the selected commit's
  secrets before the job is created (§ 6).

Before the second check, the value has been passed to `PinStore::resolve`
and the pin loader, which do not record a private value (§ 5.5), and it is
not written to the memo until it passes (§ 5.2). The memo itself has no API,
log or metric outlet; its readers are resolution and the PinStore keep-set.

**What the check does not cover.** It matches secret values verbatim. A ref
template that transforms a context value holding a secret is not caught —
the accepted risk of § 10.1 (D13). The ref template itself has no `secret`
and no `vals` (§ 4.2 rule 2), so a secret can only reach it through a value
already in its context.

**A full-SHA ref needs no extra privacy.** A 40-hex rendered value is used as
the commit itself, and `PinStore::ensure` / `ensure_tree` record the commit in
their spans (`pins.rs:957`, `:1005`) before the second check. That is safe:
the first check (live set and this job's pins) has already passed, and the
second check is against the selected commit X — whose content cannot contain
X's own hash, because the hash is computed from the content. If the SHA names
no commit, the load fails and no commit can define a secret with that value.
What remains is a secret from a pin outside this job's set, the residual
accepted above.

Why not `redaction::job_redaction_values` (the full redaction closure): it walks
the job's whole lineage (hook chains, restarts, re-runs, up to
`MAX_REDACTION_CLOSURE_JOBS`) and can load cold pins, once per deferred step,
including inside `init`, to check one string; and it has a third outcome,
`MaskAll`, with no meaning for a yes/no check. The narrower set is bounded by
the job's own pins and has two outcomes. A failure to load one of those pins
fails the step (D7). Residual, accepted: a secret that reaches a step output
from a pin outside this job's set (e.g. a child job's pin, carried up by a
`type: task` step's output) is not caught; it is masked everywhere it is
content, but not in `action_ref`.

### 10.4 Render-path table

`Scope::StepRef`, `Scope::TaskActionRef` and `render_context::webhook_ref` are
new render paths. They are added to T2 § 3.4's table as value-free paths; every error
message they produce is built from closed sets (step name, workspace name,
error kind).

## 11. Rollout

- An old replica rejects a templated ref at creation (`GitRefError::Templated`,
  400) — safer than the first `ref:` rollout, where an old replica dropped the
  key.
- But a deferred row created by a new replica can reach an old leader's
  unmatched-step sweep, which fails it after `unmatched_step_timeout_secs`; an
  old replica's `advance` never resolves it. Rule: **merge no YAML with a
  templated `ref:` before every replica runs the release.** The upgrade note
  states it.
- The same rule also covers the claim side. An old replica's `claim_job` does
  not validate capabilities, so a client holding the worker token could send
  `["deferred"]` to an old replica and claim a deferred row. Deferred rows
  exist only once YAML with a templated `ref:` is in use, so the rule keeps
  them away from old replicas. (A worker-token holder is already trusted with
  every step it claims and that step's secrets; the new validation is defence
  in depth, not a trust boundary.)
- Workers do not change: a claim never returns a deferred step.

## 12. Tests

Unit (`#[cfg(test)]`):

- `classify_ref`: static, `{{`, `{%`.
- Post-render check: trim, empty, a secret value inside, invalid name, short-SHA
  hint, a template marker in the rendered value.
- `render_context::build` for `StepRef` and `TaskActionRef`, and
  `render_context::webhook_ref`: no `secret`; `each` only for `StepRef` with a
  `loop_slot`; insertion order; collisions recorded.
- Validation: a template accepted in the three places; rejected in scheduler
  triggers, hooks, event sources, agent task tools, non-`task` actions; syntax
  errors and unknown filters.
- `KNOWN_ABILITIES` never contains `deferred`.

Integration (`crates/stroem-server/tests/git_refs_deferred_test.rs`, declared
in `tests/main.rs`; `PinnedFixture`):

- `ref: "{{ input.branch }}"`: creation writes a deferred row; promotion
  resolves it; the claim returns the branch's action and commit.
- A ref read from an earlier step's output.
- `for_each` with `{{ each.item }}`: distinct values → distinct commits; equal
  values → one commit.
- Memo: a static ref and a template rendering the same value get one commit
  after the branch moves between creation and promotion; two promotions of the
  same rendered value across a branch move get one commit.
- Failures (each fails the step with a value-free message and the job
  settles): empty value, ref not found, non-git owner, missing action,
  `type: agent`, secret in the value, `break_remote` (temporary).
- Resolved type `task` (child created from `T@ref`) and `approval` (step
  `suspended`).
- A worker with every ability never claims a deferred step; a claim request
  carrying `deferred` is a 400; the unmatched-step sweep ignores it.
- A ROOT step with a templated ref is resolved by `init` (no recovery wait);
  its failure fails the step and the job, and the execute call still returns
  the job; an agent task-tool child whose only root fails is `BornTerminal`.
- A syntax error in a templated ref is a 400 at creation.
- Phase 4.6 advances a deferred step left `ready`.
- Two concurrent `advance`s: one `UPDATE` wins, both see one commit.
- Cancel while deferred: the pass writes nothing (job-status condition).
- A failing resolution racing a successful one never overwrites the resolved,
  claimed or completed row (`fail_deferred_tx` changes no row).
- Migration 051/052: a deferred row (`action_type = 'deferred'`,
  `runner = 'none'`) inserts; the constraint is valid after 052.
- Lease: two concurrent advances — exactly one takes the lease and resolves;
  a resolve held past `DEFERRED_LEASE_SECS` (`PinStore::hold_loads_for_test`)
  keeps its lease by renewal and Phase 4.6 does not start a second one; a
  resolver whose lease was taken over cannot write (token fence) for success
  or failure; Phase 4.6 advances a row whose lease has expired.
- Private path: a `PinLoadFailed` on a rendered ref logs the commit-only label
  (`ws (sha)`); no captured span, log line, error message or HTTP body carries
  the rendered value before it passes the checks; the webhook's error log and
  response carry only `RenderedRefError` text.
- `vals` in a ref template is a validation error, and a render error at
  runtime.
- Ref projection (§ 5.6): nested string leaves render, arrays keep their
  order; a leaf that reads `secret` renders as it does in the child input;
  any leaf error fails the step value-free (no fallback to a `default`).
- A resolution failure that loses to a cancellation writes nothing (the step
  ends `cancelled`).
- Ref secret set: a value equal to a secret that exists only in the selected
  commit is rejected after the config load, and nothing is written; no span,
  log line or error text carries the rendered value (assert on captured
  `tracing` output and `job_step.error_message`).
- `type: task` action ref: the ref input is the caller's step input rendered
  as strings; a `json` field of the task still reaches the child as a native
  value; action defaults are not in the ref context.
- `type: task` action with a templated `ref:` resolved at dispatch; owner-side
  render error withheld when `O != A`.
- Webhook `{{ input.body.ref }}`: a pinned job; empty value and unknown ref →
  400; `break_remote` → 500.
- Redaction: after resolution, job detail masks the resolved pin's secrets
  (extend `read_path_audit_test.rs`).

stroem-db: migration 051; `job_ref_pin` first writer wins; the sweep clause;
`EXPLAIN` of the claim and the sweep still use `idx_job_step_ready_claim`.

E2E (`tests/e2e.sh` section 20): a task input selects the branch; the step
runs that branch's script.

UI (Vitest): the `ref:` label on a deferred step row.

## 13. Documentation

- `docs/src/content/docs/guides/git-refs.md`: section "Templated refs" (places,
  contexts, timing, empty value, the caution box of § 10.1, `stroem run`).
- `guides/templating.md`: the three new scopes.
- `operations/`: upgrade note for migration 051 and the rollout rule (§ 11).
- `CLAUDE.md` § Git refs; `CONTEXT.md`: *Deferred step*, *Ref memo*.
- `docs/internal/TODO.md` (§ 14); `docs/internal/stroem-v2-plan.md` status;
  regenerate `docs/public/llms.txt`.

## 14. Out of scope / follow-ups (TODO.md)

- An empty rendered value meaning "no ref" (D6 option A). Safe to add later:
  it turns an error into a behaviour.
- A trust gate for templated refs (`ref_allow` globs, or an owner-workspace
  policy — GR roadmap item).
- Closing the transformed-secret path (§ 10.1, D13): a restricted ref
  grammar, or masking of ref identifiers in every outlet.
- Waiting through a temporary git failure at promotion instead of failing
  (D7).
- A creation-time pre-check for templates that read only `input` (D8).
- Per-run ref override from API / UI / MCP / CLI (GR § 16) — partly covered:
  an author can expose a `branch` input.
- The claim SQL does not read the job's status, so a `ready` step can be
  claimed in the window between `JobRepo::cancel` and
  `cancel_pending_steps` (pre-existing, every step kind; § 5.7).
- Idempotent `type: task` / `type: approval` dispatch (winner-only
  `mark_running_server` / `mark_suspended`, one child per parent step, atomic
  suspend + message), and then recovery of a `ready` task / approval row left
  by a crash (§ 5.8; Codex round 2 N4/N5; lifecycle-hardening spec).
- Templated refs on hooks, event sources, agent task tools, scheduler triggers.

## Revision history

- **1 (2026-10-15)** — first draft from the brainstorming session.
- **2 (2026-10-15)** — self-review + advisor: root steps are inserted `ready`,
  so `dispatch::init` runs the pass too (§ 5.3); `claim_job` must validate
  capabilities, because the claim SQL matches the REQUEST's capabilities (D9,
  § 2, § 8); explicit order for § 5.6 (the ref renders before the task schema
  is known); the secret check uses the job's own pins, not the redaction
  closure (§ 10.3); creation runs `check_template_syntax` (§ 7); webhook
  context has its own constructor (§ 4.2).
- **3 (2026-10-15)** — Codex round 1 (thread 01a13e88): the selected commit's
  secrets are checked after its config loads (§ 5.3, § 6, § 10.3); resolution
  failures use a guarded `fail_deferred_tx`, never `mark_failed` (§ 5.5); the
  pin resolver stops recording the ref and the new paths format pin errors by
  type only (§ 5.5); Phase 4.6 also re-advances `ready` task / approval rows
  (§ 5.8); the guarded `UPDATE` checks the job's status and the cancel window
  is described as it is (§ 5.3, § 5.7); a `type: task` ref renders from a
  schema-free string render of the caller's step input (§ 5.6); restart is an
  explicit exception to criterion 4; rollout covers old-replica claims
  (§ 11); citations fixed (claim SQL, root-step status, `RefMemo` range).
- **4 (2026-10-15)** — Codex round 2: migration 051 widens
  `job_step_action_type_check` (`NOT VALID`, validated by 052) and deferred
  rows use `runner = 'none'` (§ 5.1, § 8); a rendered value is private until
  both checks pass — commit-only pin labels on a private pin-load path and one
  typed `RenderedRefError` leaving every new path (§ 5.5, § 6); no ref render
  has `secret` or `vals`, and the § 5.6 ref input is a recursive projection
  that drops failing leaves (§ 4.2, § 5.6, § 10.3); both resolution writes
  take the job row `FOR SHARE` first (§ 5.3, § 5.5); a resolution lease (D12);
  the memo is written only after the checks (§ 5.2); **scope cut**: Phase 4.6
  re-advances deferred rows only, task / approval dispatch idempotency moves
  to the lifecycle-hardening spec (§ 5.8, § 14).
- **5 (2026-10-15)** — Codex round 3: a short renewable lease with an owner
  token, and both final writes fence on it (D12, § 5.3, § 5.5, § 5.8); a
  transformed secret in a ref identifier is an accepted risk by user decision
  — a restricted grammar was offered and declined (D13, § 10.1, § 10.3; the
  rev-4 claim that such values were already visible was wrong); the § 5.6 ref
  input uses the real step-input context and fails on any leaf error, so no
  leaf is silently dropped; a full-SHA ref needs no extra privacy, with the
  reason in § 10.3 (round-3 finding 1 answered, not changed).
