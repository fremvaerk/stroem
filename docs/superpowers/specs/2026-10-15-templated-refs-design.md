# Templated refs (`ref: "{{ … }}"`)

Status: draft, revision 2 (2026-10-15)
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
2. A `type: task` action's `ref:` can read the action's prepared input.
3. A webhook trigger's `ref:` can read the webhook job input
   (`body`, `headers`, `method`, `query` and the trigger's `input` defaults).
4. Inside one job, every step that uses the same `(owner, ref)` runs the same
   commit — static and templated refs alike — even if the branch moves.
5. Static refs, and jobs without `ref:`, behave exactly as today.

## 2. Background

- **Resolution happens at job creation.** `create_job_for_task_inner`
  (`crates/stroem-server/src/job_creator.rs:414`) resolves every flow-step ref
  through `resolve_step_action` (`job_creator.rs:1291`) and a per-creation
  `RefMemo` (`job_creator.rs:1089-1145`), then builds every step row with
  `build_step` (`job_creator.rs:872`) and inserts them all at once
  (`crates/stroem-db/src/repos/job_step.rs:347`). The row carries everything
  derived from the action definition: `action_type`, `action_spec`,
  `action_image`, `runner`, `required_ability`, `required_tags`, the retry
  columns, `action_workspace`, `action_revision`, `action_ref` and the
  `task_*` columns.
- **Worker selection happens in SQL.** The claim
  (`job_step.rs:662`) matches `worker.capabilities @> to_jsonb(required_ability)`
  and `required_tags <@ worker.tags` before any Rust code sees the step. The
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

## 4. YAML surface and semantics

### 4.1 Where a template is allowed

| Place | Renders and resolves | Context (render-context insertion order) |
|---|---|---|
| Flow-step `ref:` | When the step is promoted to `ready` (§ 5.3) | `input` (job input), `state`, `global_state`, `job`, completed step entries, `each` (instances only) |
| `type: task` action `ref:` | At dispatch, after the step is `ready` (§ 5.6) | `input` (the action's prepared input: the caller's rendered step input with the action's defaults), `job` |
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
   `for_each` instance) and `Scope::TaskActionRef { prepared_input: &Value }`.
   Neither inserts `secret`. The webhook ref is rendered before any job
   exists, so `build` (which takes a `JobContext`) does not fit; a separate
   constructor in the same module, `render_context::webhook_ref(input: &Value) -> RenderContext`,
   builds a context holding only `input`. `RenderContext` stays opaque and is
   constructed only in `render_context.rs`.
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
| `action_spec`, `action_image`, `runner`, `action_revision`, `action_ref`, `task_*` | `NULL` / empty |
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
- At resolution: read `(job_id, owner, rendered)`. If absent, resolve through
  the PinStore, then `INSERT … ON CONFLICT DO NOTHING`, then read again — the
  first writer decides the commit for the rest of the job.
- Keyed by the string as written, like `RefMemo` (`release/2.3` and
  `refs/heads/release/2.3` are two keys).
- A child job (`type: task`) has its own memo, seeded at its creation, as
  today's `RefMemo` is per creation.
- `JobRepo::pin_keep_set` (`crates/stroem-db/src/repos/job.rs:848`) also keeps
  the `job_ref_pin` commits of non-terminal jobs, so a resolved commit stays
  in the PinStore until its step is claimed.

### 5.3 When the step is `ready`: `resolve_deferred_steps`

A deferred step reaches `ready` in two ways, and neither changes:

- a **root** step (no `depends_on`) is inserted `ready` at creation
  (`job_step.rs:368`);
- a **dependent** step is promoted by the cascade like any step (its `when` is
  evaluated first; a skipped step is never resolved).

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

- A resolution failure of a root step is a per-step failure (`mark_failed` +
  cascade, § 5.5), exactly like a root `type: task` dispatch failure inside
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

1. Build the context with `render_context::build(job, steps, loop_slot, Scope::StepRef)`
   from the same `Snapshots` the advance already holds (`loop_slot` set for a
   `for_each` instance).
2. Render `ref_template`; apply the post-render check (§ 4.2 rule 3).
3. Look up `(job_id, owner, rendered)` in `job_ref_pin`; on a miss, resolve
   `owner@rendered` with `PinStore::resolve` and insert (§ 5.2).
4. Load the owner's config at the commit (`config_for_user`).
5. Find the action and build the row values with the SAME code creation
   uses. `resolve_step_action` and the per-step part of
   `create_job_for_task_inner` are refactored into one function,
   `job_creator::resolve_step_row(…, git_ref_override: Option<&str>) -> NewJobStep`,
   called by creation (static refs) and by this pass (rendered refs). For a
   `type: task` action it calls `resolve_task_for_step` as creation does.
6. Run the moved creation checks: literal connection inputs against the
   resolved action's schema; `type: agent` is rejected.
7. Write the real columns in one guarded statement:
   `UPDATE job_step SET … WHERE job_id = $1 AND step_name = $2 AND status = 'ready' AND action_type = 'deferred'`.
   Zero rows → another advance, a cancel or a timeout came first; the pass
   moves on.

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
  `PinUnavailable` (D7) — fails the step with `mark_failed` and runs
  `cascade_and_settle`, as `fail_task_step` does
  (`crates/stroem-server/src/settlement/dispatch.rs:88`). It does not consume
  a retry attempt and does not go through `fail_or_retry`: the step has not
  run.
- Messages are value-free (T2 § 3.2 and the CLAUDE.md post-render rule): they
  name the step, the owner workspace and the error kind, never the rendered
  value. Example: `[ref] step 'publish': the rendered ref was not found in workspace 'billing'`.
  Template errors are already value-free (`TemplateError`).
- The full chain is scrubbed with the job's redaction values and logged with
  `tracing::error!`; the job log and `job_step.error_message` get the
  value-free message.

### 5.6 A `type: task` action with a templated `ref:`

The step is an ordinary `type: task` step. At creation `resolve_task_for_step`
is not called for it (today it would fail with `GitRefError::Templated`); its
`task_*` columns stay `NULL`. `action_spec` carries the whole action
definition — `build_step` stores `serde_json::to_value(action)`
(`job_creator.rs:886`), and `ActionDef.git_ref` serializes as `ref` when set
(`crates/stroem-common/src/models/workflow.rs:265-266`) — so the template is
on the row.

At dispatch, `handle_task_steps_pass`
(`settlement/dispatch.rs:188`) classifies `action_spec.ref`. For a template,
the order is a constraint, not a suggestion — the task's schema is only known
once the task's commit is:

1. build the action-side input as today: the caller's rendered step input
   (bucket `C`) with the action's own `input` defaults (bucket `D`, rendered
   once — spec R26). This needs only the action's persisted schema, never the
   task's;
2. render the ref with `Scope::TaskActionRef { prepared_input: C+D }` and apply
   the post-render check;
3. resolve the task owner `T` syntactically from `action_spec.task` relative
   to the action owner `O` (as `refs::ref_owner` does for a static ref), then
   `T@rendered` through the parent job's `job_ref_pin`;
4. load `T`'s config at that commit and find the task;
5. only now run `resolve_task_input_by_provenance(_roles)` against that task's
   schema, then continue exactly as a static-ref `type: task` step (`task_*`
   stamped on the parent step, child created pinned).

Failures go through `fail_task_step`. The template is written by the action
owner `O`, so a render error is withheld when `O != A` (owner-side origin,
CLAUDE.md § Cross-Workspace References); a post-render error is value-free in
every case.

A flow step with a templated `ref:` whose resolved action is a `type: task`
action with its own templated `ref:` combines the two: § 5.3 resolves the
action, § 5.6 resolves the task at dispatch.

### 5.7 What does not change

- **The claim.** A resolved row is like any other row; `ClaimResponse` gains no
  field.
- **Step retry** (`fail_or_retry`): the row is already resolved, so every
  attempt runs the same commit.
- **Restart from step**: carried rows keep their columns; steps that run again
  get new deferred rows and resolve again. The restart job's memo is seeded
  from its own creation, so a re-run step can get a newer commit than a
  carried step that rendered the same ref (same as a static ref today, which
  re-resolves at restart).
- **Task retry, re-run**: a new job with a new memo; templates resolve again.
- **Cancellation, job timeout**: a `ready` deferred step is cancelled like any
  `ready` step; the guarded `UPDATE` then matches no row.
- **Step timeout** is measured from `started_at` (claim), so resolution time
  does not count. Job timeout does.

### 5.8 Recovery

A crash between the cascade (or creation) commit and the pass leaves a
`ready` deferred step that no worker can claim and nothing else revisits. New recovery phase 4.6,
`readvance_deferred_steps` (leader only, next to Phase 4.5,
`crates/stroem-server/src/recovery.rs:338`): list non-terminal jobs that have a
step with `status = 'ready' AND action_type = 'deferred' AND ready_at < NOW() - unmatched_step_timeout_secs`
and `advance` each. Bounded and resumable like Phase 4.5
(`StalledSweepBounds`, a cursor on `AppState`, one heartbeat per job). The
guarded `UPDATE` and the first-writer memo make a concurrent resolution
harmless.

The same gap exists for `type: task` steps today (a `ready` task step not
dispatched before a crash); that is a separate TODO entry (§ 14), not fixed
here.

## 6. Webhook trigger `ref:`

The handler order is unchanged (CLAUDE.md § Git refs → Triggers): authenticate
against the cached secret, `force_refresh` if configured, re-match on one
config snapshot, re-authenticate against the fresh secret. Then:

1. Build the webhook job input (`crates/stroem-server/src/web/hooks.rs:608-645`).
2. If the trigger's `ref:` is a template, render it with the
   `render_context::webhook_ref` context (§ 4.2 rule 2) and apply the post-render check (§ 4.2 rule 3; the secret check uses the
   live redaction set).
3. Call `trigger_target::resolve_trigger_target` with the rendered value — from
   here on it is exactly a static-ref webhook.

Errors go through `classify_execute_error`: an empty value, an invalid value,
a render error and a ref that is not found are 400; `PinUnavailable` is 500.
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
- `CREATE TABLE job_ref_pin …` (§ 5.2).
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
says so in a caution box. Follow-ups: § 14.

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
  commit in the job's `job_ref_pin` — the commits this job's steps run under;
- for a webhook: the live set.

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
- Cancel while deferred: the pass writes nothing.
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
- Waiting through a temporary git failure at promotion instead of failing
  (D7).
- A creation-time pre-check for templates that read only `input` (D8).
- Per-run ref override from API / UI / MCP / CLI (GR § 16) — partly covered:
  an author can expose a `branch` input.
- Recovery for a `ready` `type: task` step that was never dispatched (§ 5.8).
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
