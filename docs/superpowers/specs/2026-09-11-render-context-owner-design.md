# One Owner for the Step Render Context — Design

Status: revision 3, proposed
Ships in: 0.16.4 (patch; carries documented behaviour changes, §6)

Addresses candidate 4 of the 11 September 2026 architecture review, which ranked
it the top recommendation. Line numbers cite `main` at `b21036b`.

## Revision history

**Revision 3 (2026-09-12).** Revision 2's central structure was wrong. It
modelled two "families" — claim-time with state, cascade-time structurally
without — and defended that as a purity boundary. It is not one:
`cascade::execute` already performs DB reads *before* calling the pure `run`
(`cascade.rs:890` precedes `pool.begin()` at `:895`), so a snapshot fetch there
keeps `run` pure — it takes the snapshot as a parameter. What revision 2 called
a boundary is an unplumbed pipe. Decision taken: plumb it. Every scope gets
state; "one owner" means one context.

The same finding exposed that the documented `when: "{{ not state or … }}"`
does not work in any form today, because the `when` context never contains
`state` (§4.1). Commit `441a6ca` changed the example from a bare expression to a
braced one and described that as a fix; it was not — both forms are always
true on the production path. This design is what makes the example true.

Other corrections from the revision 2 review, each folded in below: approval
messages fall back to job input when the step has no input mapping (§3.2);
`ContextInputs` cannot be "assembled once" because prepared step input does
not exist until after S1 plus default/connection preparation (§3.1, §3.4);
suspended rows carry real output that must stay masked (§3.3 rule 3); namespace
precedence beyond `job` was unspecified and the builders disagree (§3.3 rule 1,
§6); `null` in the context renders as `""`, not `null` (§6); always-inserting
`secret` is an observable change (§6); loop-instance entries are observable via
`__tera_context` (§6 fn 1); the security baseline postdates `b21036b` (§4.5);
the validation follow-on was self-contradictory (§7).

**Revision 2 (2026-09-11)** corrected revision 1's inherited premise that
undefined variables render empty — they error (`template.rs:82-84`,
`test_render_missing_variable:936`) — and its promise of `each` in `when:`,
which is structurally impossible (§4.2). Two defects found during that review
shipped separately as `1c31db8` (agent prompt render failures surfaced; `each`
reaches the agent context) and `b21036b` (secret values scrubbed from persisted
render errors).

## 1. Problem

"What can an author reference in a template here?" has six answers and no
owner. The same variable resolves, or hard-fails the step, depending on which
YAML field it sits in.

| # | Call site | Renders | Runs in |
|---|---|---|---|
| S1 | `rendering.rs:47-128` `render_step_input` | flow step `input:` | `claim_job` |
| S2 | `rendering.rs:211-349` `render_action_spec` | `env`/`cmd`/`script`/`source`/`manifest`/`args` | `claim_job` |
| S3 | `rendering.rs:355-405` `render_image` | `image:` | `claim_job` |
| S4 | `jobs.rs:758` → `build_step_render_context` | agent `prompt`/`system_prompt` | `claim_job` |
| S5 | `cascade.rs:678,687` → same | `when:`, `for_each:` | `cascade::run` |
| S6 | `dispatch.rs:126`, `:297` → same | `type: task` step `input:`, approval `message:` | settlement |

A seventh builder, `stroem-cli/src/local/run.rs:434`, belongs to review
candidate 3 and is out of scope; it is named because it constrains placement
(§4.4).

### 1.1 The divergence matrix

Verified by reading each site. "err" = the template hard-fails the step.

| context variable | S1 `input:` | S2 action body | S3 `image:` | S4 agent | S5 `when:` | S6 task-input / approval |
|---|:---:|:---:|:---:|:---:|:---:|:---:|
| `input` | job | prepared step | prepared step | job | job | job / **job→step if nonempty** |
| `secret` value | caller ws | **owner** ws | **owner** ws | caller ws | caller ws | caller ws |
| `secret` position | before steps | before steps | before steps | **after** steps | **after** | **after** |
| `secret` when empty | omitted | `{}` | `{}` | omitted | omitted | omitted |
| `job` | ✓ before steps | ✓ | ✓ | ✓ | ✓ | ✓ |
| statuses included | completed | completed | completed | c+s+f+susp | c+s+f+susp | c+s+f+susp |
| `.error` on failed | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ |
| `state`/`global_state` | ✓ | ✓ | **✗ err** | **✗ err** | **✗ err** | **✗ err** |
| `each` position | after steps | after | after | after *(`1c31db8`)* | **never** | after |
| loop-instance rows | included | included | included | skipped | skipped | skipped |

One row revision 1 got wrong and is **not** a divergence: `output` on a
*completed* step with NULL output is omitted by all six (`rendering.rs:105-107`,
`job_creator.rs:625-627`), so `{{ prev.output }}` errors everywhere alike.
S4–S6 insert `output: null` only for skipped/failed/suspended rows, which S1–S3
never include.

### 1.2 The author-visible bugs

1. **`{{ state.x }}` in an `image:` fails the step.** S3 has no state parameter
   (`rendering.rs:355-363`) although the caller holds both values.
2. **`{{ state.x }}` in an agent prompt fails the step.** S4 uses
   `build_step_render_context`, which has no state parameter at all.
3. **`{{ state.x }}` in a `when:` is always undefined.** So the documented
   expiry guard `when: "{{ not state or state.days_remaining < 30 }}"`
   (`task-state.md:88`) takes the `not state` branch on every run and the step
   runs unconditionally. The feature the guide describes does not exist.
4. **`{{ state.x }}` in a flow step's `input:` works for a worker step and fails
   for a `type: task` step.** Same YAML field, different call path (S1 vs S6).
5. **`{{ failed_step.error }}` works in `when:` but fails in a step `input:` or
   action body.** S1–S3 include only completed steps.
6. **A step named `secret` shadows the secrets at claim time and is shadowed by
   them at cascade time** (position row). Same for `each` vs `job`: `job` is
   inserted before steps everywhere by design, `each` after steps everywhere.
7. **Loop-instance rows** appear at claim time and not at cascade time.

### 1.3 Why the shape produces them

**Hand-threaded parameters.** S2 takes ten positional arguments and S3 eight —
seven the same values, threaded by hand at `jobs.rs:685` and `:707`. Bug 1 is
a parameter that does not exist in a signature.
`#[allow(clippy::too_many_arguments)]` at `rendering.rs:210` marks the spot.

**An open return type.** `build_step_render_context` returns
`serde_json::Value`, so callers patch variables in afterwards —
`dispatch.rs:128` and `:299` add `each`; `jobs.rs:773` now does too
(`1c31db8`), and before that the agent path simply lacked it. The missing owner
and the permissive return type are the same defect.

**Snapshot acquisition lives in one handler.** The ~70-line task+global
snapshot resolution at `jobs.rs:533-600` exists only inside `claim_job`, so
every other renderer is state-blind not by decision but by location.

### 1.4 Why the tests did not catch it

53 unit tests sit on S1–S3 in `rendering.rs` and none covers the wiring
between them. The pattern is a hand-maintained triplet asserting one property
once per builder (`test_render_{step_input,action_spec,image}_step_named_job_shadows_job_metadata`
at `:2266`, `:2293`, `:2316`; `job.revision` at `:2169`, `:2223`, `:2245`). The
state triplet is incomplete — `test_render_step_input_with_state_json`
(`:1890`) has no S2 or S3 counterpart. That missing third test is bug 1.

Bug 3 is the same failure one level up: `441a6ca` "verified" the `when`
example by calling `evaluate_condition` with a hand-built context containing
`state`. The production path never builds one. A test that measures a builder
in isolation cannot see what the caller failed to pass.

## 2. Goals and non-goals

**Goals.** One module owns context construction; every scope sees the same
variables; the two axes that are real (`which input`, `whose secrets`) are data
the module is given. Snapshot acquisition has one implementation, called from
every path that renders. Adding a variable is a one-line change in one place.

**Non-goals.** The CLI builder (candidate 3). The hook and event-source
constructors (`settlement/hooks.rs:551`, `event_source.rs:356`) — different
shapes. Per-instance `when` conditions (§4.2). The cross-workspace agent gap
(§7). Connection secrets reached via `{{ input.* }}` at cascade time
(TODO.md, pre-existing residual of `b21036b`).

## 3. Design

### 3.1 The module

New: `crates/stroem-server/src/render_context.rs`.

```rust
/// The invariant part of a step's render inputs: known before any template
/// is rendered, built once per claim or per advance, never patched.
pub struct ContextInputs<'a> {
    pub job_input: Option<&'a serde_json::Value>,
    pub caller_secrets: &'a HashMap<String, serde_json::Value>,
    pub owner_secrets: &'a HashMap<String, serde_json::Value>,
    pub steps: &'a [StepView<'a>],
    pub snapshots: &'a Snapshots,
    pub loop_slot: Option<LoopSlot<'a>>,
    pub job_revision: Option<&'a str>,
}

/// The phase-dependent part. A variant that needs a value the caller must
/// have produced first carries it, so the requirement is in the type:
/// `ActionBody` cannot be built without naming the prepared input.
pub enum Scope<'a> {
    StepInput,
    ActionBody { prepared_input: Option<&'a serde_json::Value> },
    AgentPrompt,
    Condition,
    ChildTaskInput,
    ApprovalMessage { rendered_input: Option<&'a serde_json::Value> },
}

/// Opaque: the only constructor is `build`, so no caller can patch a variable
/// in afterwards. Wraps `Secret` because the context holds rendered secrets.
pub struct RenderContext(Secret<serde_json::Value>);
impl RenderContext { pub fn as_value(&self) -> &serde_json::Value; }

pub fn build(inputs: &ContextInputs, scope: Scope) -> RenderContext;
```

`StepView` projects the five fields the context needs (`step_name`, `status`,
`output`, `error_message`, `loop_source`) with `From<&JobStepRow>`, so the
module is unit-testable without a database.

`Snapshots` is the resolved task + global state for one `(workspace, task)`:

```rust
pub struct Snapshots {
    pub task: Option<Snapshot>,     // None: no snapshot exists, or lookup failed
    pub global: Option<Snapshot>,
}
pub struct Snapshot {
    pub storage_key: String,        // for ClaimResponse
    pub has_json: bool,
    pub json: Option<serde_json::Value>,   // parsed state.json sidecar
}
```

### 3.2 The scope-dependent surface

This table is the whole `match scope`.

| `Scope` | `input` | `secret` |
|---|---|---|
| `StepInput` | job | caller |
| `ActionBody { prepared_input }` | prepared step input (S2 + S3) | **owner** |
| `AgentPrompt` | job | caller |
| `Condition` | job | caller |
| `ChildTaskInput` | job | caller |
| `ApprovalMessage { rendered_input }` | `rendered_input` if `Some` and a nonempty object, else job | caller |

`secret` is a security boundary. Action bodies resolve against the OWNER
workspace's secrets, step inputs against the CALLER's (`jobs.rs:673` vs
`rendering.rs:75-79`). Giving step-input rendering the owner's secrets would
let a caller exfiltrate a foreign workspace's secrets by templating them into
an input; connections are `shared`-gated precisely to stop that.

`input` is phase-forced. `StepInput` renders the value `ActionBody` consumes;
`prepare_step_action_input` (`rendering.rs:139-208`, defaults + connection
provenance) runs between them and is **not** absorbed — it needs workspace,
task, step and owner lookups that are not template concerns, and the
regression at `integration_test.rs:2668` guards it, not context assembly.
The claim sequence becomes:

```
inputs   = ContextInputs { … }                              // once
raw      = render_step_input(build(&inputs, StepInput), flow_step)
prepared = prepare_step_action_input(raw, &prep_ctx)        // unchanged
spec     = render_action_spec(build(&inputs, ActionBody { prepared_input }), spec)
image    = render_image(the same context, image)
prompts  = render_agent_prompts(build(&inputs, AgentPrompt), action)
```

`ApprovalMessage` preserves the existing two-phase behaviour at
`dispatch.rs:317-338`: the flow input is rendered against a `ChildTaskInput`
context; if the result is a nonempty object it replaces `input`, otherwise
the message sees job input. Revision 2 mapped this to an unconditional
"rendered step input" and would have broken `{{ input.foo }}` in every
approval whose step has no input mapping.

`AgentPrompt` keeps job input (today's behaviour). Switching it to prepared
step input is a plausible follow-on but is a behaviour change with no bug
behind it; deferred (§7).

### 3.3 The unconditional rules

Identical in all six scopes.

1. **Insertion order is: `input`, `secret`, `state`, `global_state`, `job`,
   `each`, then step entries.** A step whose sanitized name collides with a
   framework key therefore shadows it — the rule already documented for
   `job`, now applied uniformly. This changes two existing behaviours (§6):
   at cascade time a step named `secret` is now shadowed *by the step*
   (today the secrets win), and at claim time a step named `each` now shadows
   the loop variable (today `each` wins). Reserved-name validation is the
   right long-term fix (§7).
2. Step entries for **completed, skipped, failed and suspended** rows.
3. `output` on every entry: the stored output for **completed** rows (`null`
   when NULL); **`null` for skipped, failed and suspended rows regardless of
   stored output.** The status masking in `job_creator.rs:631-648` survives —
   a suspended approval's row holds `{"approval_message": …}`
   (`dispatch.rs:393`) and must not surface as `.output`.
4. `error` on failed rows.
5. `each` (`{item, index, total}`) whenever the row is a loop instance. Never
   for `Condition` (§4.2).
6. Loop-instance rows skipped; only the placeholder's aggregate appears.
7. `secret` always inserted, even when the map is empty.
8. `state` and `global_state`: **presence semantics** — inserted when the
   snapshot has parsed JSON, omitted otherwise. This is what makes
   `{{ not state or … }}` a working "no snapshot yet" test (§4.1).

### 3.4 Snapshot acquisition

One function, replacing the block at `jobs.rs:533-600`:

```rust
// crates/stroem-server/src/state_storage.rs
pub async fn latest_snapshots(
    pool: &PgPool,
    storage: Option<&StateStorage>,
    workspace: &str,
    task_name: &str,
) -> Snapshots
```

Best-effort, exactly as today: a lookup or archive error is logged at `warn`
and yields `None` for that snapshot; it never fails the caller. `storage:
None` (no `state_storage` configured) yields `Snapshots::default()`.

Called from four places, each before any transaction it participates in
opens:

| Caller | When | Note |
|---|---|---|
| `claim_job` | once, replacing the inline block | also feeds `ClaimResponse.state_storage_key`/`has_json` |
| `Settlement::advance` | once at the top of the non-terminal branch | threaded to `cascade_and_settle` → `execute` → `run`, and to `handle_task_steps` / `handle_approval_steps` |
| `dispatch::init` | once | creation-time cascade |
| `cascade::execute` | — | receives `&Snapshots`; does **not** fetch |

`Settlement` gains `state_storage: Option<Arc<StateStorage>>` from
`AppState.state_storage`. `cascade::run` gains a `&Snapshots` parameter and
stays pure. `execute` fetches nothing: the retry loop (`MAX_ATTEMPTS = 3`,
`cascade.rs:862`) re-reads rows but reuses the snapshot passed in.

**Cost.** Two indexed point queries (`TaskStateRepo::get_latest`,
`WorkspaceStateRepo::get_latest`) per `advance` for every job, whether or not
the task uses state, plus an archive fetch only when a snapshot with a JSON
sidecar exists. No gate in this revision: measure with the existing
Prometheus RED metrics after release and add a "task references `state`"
pre-check only if it shows. Recorded in §8.

**Consistency.** Each render sees the latest snapshot at its own moment; a
`when` evaluated at cascade time and a `script:` rendered at claim time can
observe different snapshots if an upload lands between them. That is the
existing claim-time semantics ("state resolved at claim time — enables
intra-job propagation for sequential steps", CLAUDE.md) extended one hop, and
the same race the architecture review's red band already lists for
template-vs-mount. Not new; not closed here.

### 3.5 Call-site changes

S1–S3 lose their context assembly and long parameter lists: S2 goes from ten
positional parameters to two, S3 from eight to two; the
`#[allow(clippy::too_many_arguments)]` is deleted. `cascade.rs:678,687`,
`dispatch.rs:126,297` and `jobs.rs:758` call `build` and **delete** their
post-hoc `each` patching (`dispatch.rs:128`, `:299`, `jobs.rs:773`).
`build_step_render_context` is deleted; `job_context` moves into the module.
The three secret scrubs from `b21036b` (`jobs.rs:378`, `cascade.rs:703`,
`dispatch.rs:75`) are untouched — they act on error strings after rendering,
not on the context.

## 4. Decisions

### 4.1 Presence semantics for state, and the guide's example

`state`/`global_state` are inserted only when a parsed snapshot exists. With
uniform availability this is what makes `when: "{{ not state or
state.days_remaining < 30 }}"` (task-state.md) a genuine expiry guard:
`not state` is true only when no snapshot has been written. Measured against
`evaluate_condition` with the context this design will actually build:

| snapshot | result |
|---|---|
| none | true — runs, first time |
| `days_remaining: 60` | false — skipped |
| `days_remaining: 10` | true — runs |

Until this ships, that example is false in production and `441a6ca` did not
change that. The guide gets an interim note (same commit as this revision)
saying `state` is not yet visible in `when:`, removed when 0.16.4 ships.

### 4.2 `each` in `Condition` is not deliverable

`cascade.rs:546` evaluates the placeholder's `when` before `:565` parses the
collection, and instances are created with `when_condition: None` (`:617`).
`Scope::Condition` never receives a `LoopSlot`; the module documents this as a
property of the execution model. Per-instance conditions need their own
design.

### 4.3 One context, not two families

Revision 2's `Snapshots::Unavailable` and `Scope::is_claim_time()` are gone.
There is no availability axis to enforce because there is no unavailable
scope. This also retires the "task state is not available when rendering
`<field>`" diagnostic revision 2 promised and could not deliver through
`render_template`'s generic error.

### 4.4 Placement

`stroem-server`. Moving to `stroem-common` to share with the CLI front-runs
candidate 3. `build` is a pure function of plain data, so lifting it later is
a move. `latest_snapshots` is server-only by nature (it needs the archive).

### 4.5 Secrets

`RenderContext` wraps `Secret<serde_json::Value>` per CLAUDE.md's "Secrets in
logs" rule. Value scrubbing of persisted render *errors* is already in place
at three choke points since `b21036b` and is unaffected by this design; the
residual (connection secrets reached via `{{ input.* }}` at cascade time) is
tracked in TODO.md and is not made better or worse here.

## 5. Testing

**The anti-regression test.** One table-driven test over every `Scope`
variant asserting every rule-1 key is present (with `each` exempted for
`Condition`, named explicitly) and that step entries follow them. Replaces the
hand-maintained triplets of §1.4 and would have caught bug 1.

**Per-rule unit tests** for §3.3, once each: shadowing for every framework
key, `null` for non-completed rows *including a suspended row with stored
output*, `.error` on failed, loop-instance rows skipped, `secret` present
when empty, `state` omitted when no snapshot.

**Scope-axis tests** for §3.2, including `ApprovalMessage` with `None`, with
`Some({})` and with `Some(nonempty)`.

**Snapshot tests.** `latest_snapshots` with no storage configured, no
snapshot, a snapshot without JSON, with JSON, and an archive error (yields
`None`, does not fail).

**Wiring tests — the lesson of `441a6ca`.** Each of the four
`latest_snapshots` callers gets an integration assertion that a snapshot's
value reaches the rendered field on the production path: `{{ state.x }}` in a
`when:` (cascade), in a `type: task` step input (dispatch), in an `image:`
and an agent prompt (claim). Each verified by reverting the wiring and
observing the failure, as was done for `1c31db8`. `integration_test.rs:3000`
(`job.revision` threading) stays.

**Existing coverage.** The 53 `rendering.rs` tests migrate; net count falls as
triplets collapse. `build_step_render_context`'s callers in
`orchestrator_test.rs`, `integration_test.rs`, `mcp_test.rs` move to `build`.

## 6. Behaviour changes

Not all additive. Each row says which sites change.

| Change | Sites | Today | After |
|---|---|---|---|
| `{{ state.x }}` in `image:`, agent prompt | S3, S4 | step fails | resolves |
| `{{ state.x }}` in `when:`, `for_each:` | S5 | undefined → `not state` always true | resolves; the guide's expiry example works |
| `{{ state.x }}` in task-step input, approval message | S6 | step fails | resolves |
| `{{ failed.error }}` in `input:`/action body | S1–S3 | step fails | resolves |
| skipped/failed/suspended refs in `input:`/action body | S1–S3 | step fails | `.output` renders `""` (Tera renders `null` as empty; in `when:` that is falsy) |
| `{% if secret is defined %}` with no workspace secrets | S1, S4–S6 | false | true |
| **a step named `secret`, at cascade time** | S4–S6 | secrets win | **step wins** |
| **a step named `each`, at claim time** | S1–S3 | loop var wins | **step wins** |
| **a skipped/failed/suspended step named `job`, at claim time** | S1–S3 | `{{ job.revision }}` works | **shadowed** |
| loop-instance entries at claim | S1–S3 | present | absent¹ |
| approval `{{ input.foo }}` with no step input mapping | S6 | job input | job input (unchanged, now specified) |

¹ Instance names are `format!("{}[{}]", ..)` (`cascade.rs:605`); Tera parses
`{{ process[0] }}` as indexing into `process`, so the keys cannot be
referenced by name. They *are* observable through Tera's `{{ __tera_context }}`
dump (tera 1.20.1 `processor.rs:21`). No workflow in the repository uses
either; called out rather than claimed unreachable.

The three bold rows are regressions for workflows with a step named `secret`,
`each` or `job`. All three names are framework keys; the release note says so
and §7 proposes rejecting them at validation.

## 7. Follow-ons this enables

- **Reserved step names.** With one insertion order there is one list of
  reserved names (`input`, `secret`, `state`, `global_state`, `job`, `each`).
  Validation should reject a flow step with any of them, which retires the
  three bold rows in §6 as a class. Belongs with candidate 2.
- **Availability validation** shrinks to one rule: `each` is unavailable in
  `when:` (§4.2). Note this cannot check that a runtime snapshot *contains*
  `x` — only that the scope can carry `state` at all — which after this design
  every scope can.
- **`AgentPrompt` input** could switch to prepared step input for symmetry
  with `ActionBody`. Behaviour change; own decision.
- **Cross-workspace agent steps** still render against the caller's config;
  not a single-row change (MCP selection at `jobs.rs:795` and task-tool
  creation at `jobs.rs:1106` resolve against the caller independently). This
  design gives the prompt half one place to live.
- **Snapshot fetch gate**, if the two extra queries per `advance` show in
  metrics (§3.4).

## 8. Risks

**Two extra queries per `advance`.** Accepted without a gate; measured after
release. The archive fetch is bounded to tasks that actually have a JSON
snapshot.

**Settlement grows a field.** `state_storage` joins `Settlement`; it is the
narrow-capability struct the review praised, and this is the first addition
since it was introduced. Watch for it becoming `AppState` again.

**Three named-step regressions.** Rare names, framework keys, release-noted,
and validation should close them. Not silent: a workflow hitting one changes
which value a template sees, which is why they are listed rather than
absorbed.

**Merge surface.** `rendering.rs`, `job_creator.rs`, `worker_api/jobs.rs`,
`cascade.rs`, `settlement/{mod,settle,dispatch}.rs`, `state_storage.rs`,
`state.rs`. Land as one change; rebase rather than merge.
