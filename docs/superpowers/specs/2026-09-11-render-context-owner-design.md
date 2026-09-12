# One Owner for the Step Render Context — Design

Status: revision 4, proposed
Ships in: 0.16.4 (patch; additive migration 047; documented behaviour changes, §6)

Addresses candidate 4 of the 11 September 2026 architecture review, which ranked
it the top recommendation. Line numbers cite `main` at `fba4e87`.

## Revision history

**Revision 4 (2026-09-12).** Revision 3's review closed 12 of its 13 findings
and raised nine more. Two changed the design:

- *The parsed `state.json` lives only in the archive.* `has_json` is a flag the
  uploader sends (`state.rs:135`, `:430`; `state_upload.rs:117`); the server
  never persists the sidecar, so every renderer that wants `state` must
  download the tarball, gunzip it and walk it (`extract_state_json`,
  `state.rs:196-215`). Revision 3 called that "an archive fetch only when a
  snapshot exists" and proposed doing it on every `advance`. Decision taken:
  persist the sidecar in the database (migration 047, §3.4). Snapshot
  resolution becomes two indexed point queries with no archive access, which
  also removes the need to thread `StateStorage` through the creator and the
  failure-orchestration path — the plumbing gap the review found.
- *`ContextInputs` cannot be built once per advance.* The cascade rebuilds its
  context between phases and passes from mutated working rows
  (`cascade.rs:674`, `:687`), and dispatch renders several loop instances in
  one pass, each with its own `each`. Split into a per-entry `JobContext` and
  per-render arguments (§3.1).

Also folded in: snapshot error contract preserves `storage_key`/`has_json` on
archive failure so the worker still downloads (§3.4); the freshness claim is
replaced by the guarantee actually offered (§3.4); the collision accounting is
a full key × scope table, the insertion order is chosen to minimise it, and
`build` reports collisions (§3.3, §6); "`not state`" is stated precisely
(§4.1); the caller/test inventories are corrected (§3.4, §5); two truncated
ranges fixed.

**Revision 3 (2026-09-12)** withdrew revision 2's "two families": the cascade
*can* have state, because `execute` already does DB reads before calling the
pure `run` (`cascade.rs:890` precedes `pool.begin()` at `:895`). Decision:
every scope gets state. The same finding showed the documented
`when: "{{ not state or … }}"` guard never worked — the `when` context has no
`state` — and that `441a6ca` did not fix it.

**Revision 2 (2026-09-11)** corrected revision 1's premise that undefined
variables render empty (they error: `template.rs:82-84`,
`test_render_missing_variable:936`) and its promise of `each` in `when:`
(§4.2). Two defects found during review shipped separately: `1c31db8` (agent
prompt render failures surfaced; `each` reaches the agent context) and
`b21036b` (secret values scrubbed from persisted render errors).

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
candidate 3 and is out of scope; it constrains placement (§4.4).

### 1.1 The divergence matrix

Verified by reading each site. "err" = the template hard-fails the step.

| context variable | S1 `input:` | S2 action body | S3 `image:` | S4 agent | S5 `when:` | S6 task-input / approval |
|---|:---:|:---:|:---:|:---:|:---:|:---:|
| `input` | job | prepared step | prepared step | job | job | job / **job→step if nonempty** |
| `secret` value | caller ws | **owner** ws | **owner** ws | caller ws | caller ws | caller ws |
| `secret` position | before steps | before | before | **after** steps | **after** | **after** |
| `secret` when empty | omitted | `{}` | `{}` | omitted | omitted | omitted |
| `job` | before steps | before | before | before | before | before |
| statuses included | completed | completed | completed | c+s+f+susp | c+s+f+susp | c+s+f+susp |
| `.error` on failed | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ |
| `state`/`global_state` | ✓ | ✓ | **✗ err** | **✗ err** | **✗ err** | **✗ err** |
| `each` position | after steps | after | after | after *(`1c31db8`)* | **never** | after |
| approval's rendered `input` | — | — | — | — | — | after steps |
| loop-instance rows | included | included | included | skipped | skipped | skipped |

`output` on a *completed* step with NULL output is omitted by all six
(`rendering.rs:105-107`, `job_creator.rs:625-627`); S4–S6 insert
`output: null` only for skipped/failed/suspended rows, which S1–S3 never
include. Not a divergence.

### 1.2 The author-visible bugs

1. `{{ state.x }}` in an `image:` fails the step. S3 has no state parameter
   (`rendering.rs:355-363`) although the caller holds both values.
2. `{{ state.x }}` in an agent prompt fails the step. S4's builder has no
   state parameter at all.
3. `{{ state.x }}` in a `when:` is always undefined, so the documented expiry
   guard `when: "{{ not state or state.days_remaining < 30 }}"`
   (`task-state.md:88`) runs the step on every pass. The feature the guide
   describes does not exist.
4. `{{ state.x }}` in a flow step's `input:` works for a worker step (S1) and
   fails for a `type: task` step (S6). Same YAML field, different call path.
5. `{{ failed_step.error }}` works in `when:` and fails in a step `input:` or
   action body.
6. A step named `secret` shadows the secrets at claim time and is shadowed by
   them at cascade time; a step named `each` is shadowed by the loop variable
   everywhere. Every framework key has a different collision story.
7. Loop-instance rows appear at claim time and not at cascade time.

### 1.3 Why the shape produces them

**Hand-threaded parameters.** S2 takes ten positional arguments and S3 eight —
seven the same values, threaded by hand at `jobs.rs:685` and `:707`. Bug 1 is
a parameter that does not exist in a signature
(`#[allow(clippy::too_many_arguments)]`, `rendering.rs:210`).

**An open return type.** `build_step_render_context` returns
`serde_json::Value`, so callers patch variables in afterwards
(`dispatch.rs:128`, `:299`, `jobs.rs:773`). The missing owner and the
permissive return type are the same defect.

**Snapshot resolution lives in one handler and needs the archive.** The
~70-line task+global resolution at `jobs.rs:533-606` exists only in
`claim_job`, and it must exist there because the sidecar is only in the
tarball. Every other renderer is state-blind by location, not decision.

### 1.4 Why the tests did not catch it

53 unit tests sit on S1–S3 in `rendering.rs`; none covers the wiring between
them. The pattern is a hand-maintained triplet per property
(`test_render_{step_input,action_spec,image}_step_named_job_shadows_job_metadata`
at `:2266`, `:2293`, `:2316`; `job.revision` at `:2169`, `:2223`, `:2245`).
The state triplet is incomplete — `test_render_step_input_with_state_json`
(`:1890`) has no S2 or S3 counterpart. That missing third test is bug 1.

Bug 3 is the same failure one level up: `441a6ca` "verified" the `when`
example by calling `evaluate_condition` with a hand-built context containing
`state`. The production path never builds one.

## 2. Goals and non-goals

**Goals.** One module owns context construction; every scope sees the same
variables; the two real axes (`which input`, `whose secrets`) are data the
module is given. Snapshot resolution has one implementation, needs only a
pool, and is called from every entry that renders. Adding a variable is a
one-line change in one place.

**Non-goals.** The CLI builder (candidate 3). Hook and event-source
constructors (`settlement/hooks.rs:551`, `event_source.rs:356`). Per-instance
`when` (§4.2). The cross-workspace agent gap (§7). Connection secrets reached
via `{{ input.* }}` at cascade time (TODO.md, residual of `b21036b`).
Reserved-name validation (§7).

## 3. Design

### 3.1 The module

New: `crates/stroem-server/src/render_context.rs`.

```rust
/// Per entry (one advance, one init, one claim, one failure orchestration):
/// what every render in that entry shares. Cheap — all borrows.
pub struct JobContext<'a> {
    pub job_id: Uuid,                       // for collision warnings only
    pub job_input: Option<&'a serde_json::Value>,
    pub caller_secrets: &'a HashMap<String, serde_json::Value>,
    pub owner_secrets: &'a HashMap<String, serde_json::Value>,
    pub snapshots: &'a Snapshots,
    pub job_revision: Option<&'a str>,
}

/// Per render: the rows as they are *now* and the loop slot of *this* step.
pub fn build(
    job: &JobContext,
    steps: &[StepView],
    loop_slot: Option<LoopSlot>,
    scope: Scope,
) -> RenderContext;

/// A variant that needs a value the caller must have produced first carries
/// it: `ActionBody` cannot be built without naming the prepared input.
pub enum Scope<'a> {
    StepInput,
    ActionBody { prepared_input: Option<&'a serde_json::Value> },
    AgentPrompt,
    Condition,
    ChildTaskInput,
    ApprovalMessage { rendered_input: Option<&'a serde_json::Value> },
}

/// Opaque: the only constructor is `build`. Wraps `Secret` because the
/// context holds rendered secrets.
pub struct RenderContext {
    value: Secret<serde_json::Value>,
    collisions: Vec<String>,   // step names that shadowed a framework key
}
impl RenderContext {
    pub fn as_value(&self) -> &serde_json::Value;
    pub fn collisions(&self) -> &[String];
}
```

`StepView` projects `step_name`, `status`, `output`, `error_message`,
`loop_source` with `From<&JobStepRow>`; the cascade builds views from its
working rows (`snap.rows`) so phase B sees phase A's changes exactly as today.

`Snapshots` is the resolved task + global state for one `(workspace, task)`:

```rust
#[derive(Default)]
pub struct Snapshots { pub task: Option<Snapshot>, pub global: Option<Snapshot> }
pub struct Snapshot {
    pub storage_key: String,            // ClaimResponse; worker download
    pub has_json: bool,
    pub json: Option<serde_json::Value>, // the persisted sidecar, if any
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
`rendering.rs:75-79`); giving step-input rendering the owner's secrets would
let a caller exfiltrate a foreign workspace's secrets by templating them into
an input. Connections are `shared`-gated precisely to stop that.

`input` is phase-forced. `StepInput` renders the value `ActionBody` consumes;
`prepare_step_action_input` (`rendering.rs:139-208`) runs between them and is
**not** absorbed — it needs workspace/task/step/owner lookups that are not
template concerns, and `integration_test.rs:2668` guards it. The claim
sequence:

```
job      = JobContext { … }                                          // once
views    = steps.iter().map(StepView::from)                          // once
raw      = render_step_input(build(&job, &views, slot, StepInput), flow_step)
prepared = prepare_step_action_input(raw, &prep_ctx)                 // unchanged
body     = build(&job, &views, slot, ActionBody { prepared_input: prepared.as_ref() })
spec     = render_action_spec(&body, spec);  image = render_image(&body, image)
prompts  = render_agent_prompts(build(&job, &views, slot, AgentPrompt), action)
```

`ApprovalMessage` preserves `dispatch.rs:317-338`: the flow input is rendered
against a `ChildTaskInput` context; if the result is a nonempty object it is
inserted **after** step entries (so the mapping wins over a step named
`input`, as today — rule 1), otherwise the message sees job input.

`AgentPrompt` keeps job input (today's behaviour); switching it to prepared
input is a follow-on (§7).

### 3.3 The unconditional rules

Identical in all six scopes.

1. **Insertion order.** `input`, `secret`, `state`, `global_state`, `job`,
   then step entries, then `each`, then (`ApprovalMessage` only) the
   rendered input. So a step whose sanitized name is one of the first five
   keys shadows it; `each` and approval's mapping shadow a step. This is
   today's majority behaviour per key (§1.1 rows "position"), chosen to
   change the fewest sites: only `secret` in S4–S6 flips (secrets won there;
   the step wins now), plus the broadened-status rows in §6. `build` records
   every collision in `RenderContext::collisions()`; callers append a
   `[render] step '<name>' shadows template variable '<key>'` line to the job
   log and `warn!`. Rejecting these names at validation is the class fix
   (§7).
2. Step entries for **completed, skipped, failed and suspended** rows.
3. `output` on every entry: the stored output for **completed** rows (`null`
   when NULL); **`null` for skipped, failed and suspended rows regardless of
   stored output** — the masking at `job_creator.rs:631-654` survives; a
   suspended approval's row holds `{"approval_message": …}`
   (`dispatch.rs:393`) and must not surface as `.output`.
4. `error` on failed rows.
5. `each` (`{item, index, total}`) whenever `loop_slot` is `Some`. Never for
   `Condition` (§4.2).
6. Loop-instance rows skipped; only the placeholder's aggregate appears.
7. `secret` always inserted, even when empty.
8. `state` / `global_state`: **presence semantics** — inserted when
   `Snapshot.json` is `Some`, omitted otherwise (§4.1).

### 3.4 Snapshot persistence and resolution

**Migration 047 — persist the sidecar.** `task_state` and `workspace_state`
gain `state_json JSONB NULL`. The four upload sites already hold the bytes:

| Site | Source of `state_json` |
|---|---|
| worker task upload, `state.rs:~423` | `extract_state_json(&body)` when `query.has_json` |
| worker global upload, `state.rs:~127` | same |
| API task upload, `state_upload.rs:511` | `files["state.json"]`, already unpacked (`:117`) |
| API global upload, `state_upload.rs:~600` | same |

`insert_and_prune` (both repos) gains `state_json: Option<serde_json::Value>`;
`TaskStateRow` / `WorkspaceStateRow` and `get_latest` gain the column.
`has_json` keeps its meaning (the tarball carries a sidecar) so the worker
download path is unchanged. Additive and nullable: a pre-047 server replica
ignores the column; a mixed fleet during rollout is safe.

**Resolution — pool only.**

```rust
// crates/stroem-server/src/render_context.rs
pub async fn latest_snapshots(pool: &PgPool, workspace: &str, task_name: &str) -> Snapshots
```

Two indexed point queries (`TaskStateRepo::get_latest`,
`WorkspaceStateRepo::get_latest`, `idx_task_state_lookup` and its global
twin). No archive access. Error contract: a lookup error is logged at `warn`
and yields `None` for that snapshot; a row is always returned whole, so
`storage_key` and `has_json` reach `ClaimResponse` whether or not `json` is
present — the worker keeps downloading after any server-side JSON problem,
exactly as `jobs.rs:539-556` behaves today.

**Legacy rows.** A row with `has_json = true` and `state_json IS NULL`
predates 047. `claim_job` — the one entry that has the archive — backfills
it: retrieve, `extract_state_json`, `UPDATE … SET state_json` (idempotent;
a concurrent backfill writes the same value). Until the first claim of any
step in that task, cascade-time renders see no `state` for that snapshot; the
gap is one claim wide and self-heals. Recorded in the release note.

**Entries.** Every entry that renders resolves once, at its start, and threads
`&Snapshots` down. `cascade::execute`, `run`, `handle_task_steps`,
`handle_approval_steps`, `fail_task_step` and
`orchestrate_after_server_step_failure` take `&Snapshots` and never resolve.

| Entry | Resolves | Threads to |
|---|---|---|
| `Settlement::advance` (non-terminal branch, `mod.rs:214`) | once | `cascade_and_settle` → `execute` → `run`; `handle_task_steps` → `fail_task_step` → `orchestrate_after_server_step_failure` → `cascade_and_settle`; `handle_approval_steps` |
| `dispatch::init` (`dispatch.rs:537`) | once — gains `task_name: &str` from the creator's `task_ref` (one call site, `job_creator.rs:486`) | `execute`, `handle_task_steps`, `handle_approval_steps` |
| `claim_job` | once, replacing `jobs.rs:533-606` | S1–S4; also backfills |

`Settlement` gains no field: the render path needs no `StateStorage`.

**What is guaranteed about freshness.** One sample per entry. A `when`
evaluated in an `advance` and the `script:` rendered at the subsequent claim
can see different snapshots if an upload lands between them — the existing
claim-time semantics extended one hop. Within an entry: guard-miss retries in
`execute` (`MAX_ATTEMPTS = 3`, `cascade.rs:862`) reuse the entry's sample;
nested advances reached through `reconcile` → child `advance` → `propagate`
→ parent `advance` (`mod.rs:447`, `propagate.rs:150`) each sample
independently, so an outer entry can finish on a sample older than an inner
one took. Snapshots are immutable and monotone, so "older" means "the
previous snapshot", never a torn read. Not a guarantee of latest-at-render;
stated so nobody relies on one.

**Cost.** Two point queries per entry for every job. Observed via a new
histogram `stroem_snapshot_resolve_seconds{entry}` (settlement is outside
the RED middleware, which covers `/api` only — `web/mod.rs:95`), documented
in `operations/metrics.md` per the metrics pattern. A "task references
`state`" pre-check is the fallback if it shows.

### 3.5 Call-site changes

S1–S3 lose their context assembly and long parameter lists: S2 from ten
positional parameters to two, S3 from eight to two; the
`#[allow(clippy::too_many_arguments)]` is deleted. `cascade.rs:678,687`,
`dispatch.rs:126,297` and `jobs.rs:758` call `build` and delete their
post-hoc `each` patching (`dispatch.rs:128`, `:299`, `jobs.rs:773`).
`build_step_render_context` is deleted with its unit tests at
`job_creator.rs:792-995`, which migrate to the new module; `job_context`
moves there. The three secret scrubs from `b21036b` (`jobs.rs:378`,
`cascade.rs:703`, `dispatch.rs:75`) act on error strings after rendering and
are untouched.

## 4. Decisions

### 4.1 Presence semantics for state, and the guide's example

`state` is inserted only when `Snapshot.json` is `Some`. `not state` is
therefore true when **no parsed sidecar is available**: no snapshot; a
snapshot uploaded without `has_json`; or a legacy row not yet backfilled
(§3.4). It is *not* a test for "no snapshot was ever written" — a task that
stores files but never emits `STATE:` always takes that branch, which is the
correct reading of "no structured state". With that meaning, the guide's
expiry guard works: measured against `evaluate_condition` with the context
`build` produces —

| sidecar | `{{ not state or state.days_remaining < 30 }}` |
|---|---|
| none | true — runs |
| `days_remaining: 60` | false — skipped |
| `days_remaining: 10` | true — runs |

Until this ships the example is false in production; the guide carries an
interim caution (`task-state.md:114-120`), removed with 0.16.4.

### 4.2 `each` in `Condition` is not deliverable

`cascade.rs:546` evaluates the placeholder's `when` before `:565` parses the
collection, and instances are created with `when_condition: None` (`:617`).
Per-instance conditions need their own design.

### 4.3 One context, not two families

No availability axis exists, so nothing enforces one. The diagnostic revision
2 promised ("state is not available when rendering `<field>`") is retired.

### 4.4 Placement

`stroem-server`. `build` is a pure function of plain data, so lifting it to
`stroem-common` for the CLI later (candidate 3) is a move. `latest_snapshots`
is server-only by nature.

### 4.5 Secrets

`RenderContext` wraps `Secret<serde_json::Value>` per CLAUDE.md's "Secrets in
logs" rule. The persisted-error scrubs from `b21036b` are unaffected; the
connection-secret residual is unchanged.

## 5. Testing

**The anti-regression test.** One table-driven test over every `Scope`
variant asserting every rule-1 key precedes step entries and `each` follows
them (with `Condition` exempted from `each`, named explicitly). Replaces the
triplets of §1.4; would have caught bug 1.

**Per-rule unit tests** for §3.3, once each, including: every framework key's
collision recorded in `collisions()`; `null` for a suspended row that has
stored output; `.error` on failed; loop-instance rows skipped; `secret`
present when empty; `state` omitted when `json` is `None`.

**Scope-axis tests** for §3.2, including `ApprovalMessage` with `None`,
`Some({})` and `Some(nonempty)` — the last asserting the mapping beats a step
named `input`.

**Persistence tests.** Each of the four upload sites writes `state_json`
(integration, one per site). `latest_snapshots`: no rows, row without JSON,
row with JSON, a lookup error yields `None` without failing. Legacy backfill
on claim: a row with `has_json` and NULL `state_json` is populated by the
first claim and read by the next `advance`.

**Wiring tests — one per entry, the lesson of `441a6ca`.** A snapshot value
must reach the rendered field on the production path from each entry:
`advance` (`{{ state.x }}` in a `when:` on a step promoted after a
completion), `init` (`{{ state.x }}` in a root step's `when:` — the guide's
`check-expiry` shape), `claim_job` (`{{ state.x }}` in an `image:` and an
agent prompt), and the failure path (`{{ state.x }}` in a `when:` evaluated
after a `type: task` dispatch failure). Each verified by reverting the wiring
and observing the failure. `integration_test.rs:3000` stays.

**Existing coverage.** The 53 `rendering.rs` tests migrate; net count falls as
triplets collapse.

## 6. Behaviour changes

Not all additive.

| Change | Sites | Today | After |
|---|---|---|---|
| `{{ state.x }}` in `image:`, agent prompt | S3, S4 | step fails | resolves |
| `{{ state.x }}` in `when:`, `for_each:` | S5 | undefined → `not state` always true | resolves; the guide's guard works |
| `{{ state.x }}` in task-step input, approval message | S6 | step fails | resolves |
| `{{ failed.error }}` in `input:`/action body | S1–S3 | step fails | resolves |
| skipped/failed/suspended refs in `input:`/action body | S1–S3 | step fails | `.output` renders `""` (`null` through Tera; falsy in `when:`) |
| `{% if secret is defined %}`, no workspace secrets | S1, S4–S6 | false | true |
| loop-instance entries at claim | S1–S3 | present | absent¹ |
| approval `{{ input.foo }}` with no step mapping | S6 | job input | job input (unchanged, now specified) |
| legacy snapshot, first cascade after upgrade | S5, S6 | — | no `state` until the next claim backfills |

**Collisions** — a step whose sanitized name equals a framework key. "step"
= the step entry is what the template sees; "key" = the framework value is.

| step named… | S1–S3 today → after | S4 today → after | S5 today → after | S6 today → after |
|---|---|---|---|---|
| `input` | step → step (completed); **key → step** for s/f/susp rows | step → step | step → step | step → step; approval mapping still wins |
| `secret` | step → step; **key → step** for s/f/susp | **key → step** | **key → step** | **key → step** |
| `state`, `global_state` | step → step; **key → step** for s/f/susp | n/a → step | n/a → step | n/a → step |
| `job` | step → step; **key → step** for s/f/susp | step → step | step → step | step → step |
| `each` | key → key | key → key | n/a | key → key |

Bold cells are regressions: `secret` at cascade time (S4–S6), and in S1–S3 any
non-completed step named `input`/`secret`/`state`/`global_state`/`job`, which
today is filtered out (`jobs.rs:524`) and now shadows. Every one is reported
by `collisions()` at render time and in the release note; §7 proposes
rejecting the names.

¹ Instance names are `format!("{}[{}]", ..)` (`cascade.rs:605`); Tera parses
`{{ process[0] }}` as indexing into `process`, so the keys cannot be named,
but they are observable through `{{ __tera_context }}` (tera 1.20.1
`processor.rs:21`). No workflow in the repository uses either.

## 7. Follow-ons

- **Reserved step names** (`input`, `secret`, `state`, `global_state`, `job`,
  `each`): reject at validation; retires the collision table as a class.
  Belongs with candidate 2 — validation is not wired into server load today.
- **Availability validation** shrinks to one rule: `each` is unavailable in
  `when:`.
- **`AgentPrompt` input** could switch to prepared step input for symmetry
  with `ActionBody`. Behaviour change; own decision.
- **Cross-workspace agent steps** still render against the caller's config;
  MCP selection at `jobs.rs:795` and task-tool creation at `jobs.rs:1106`
  resolve against the caller independently. This design gives the prompt
  half one place to live.
- **Backfill job** for legacy snapshots, if the one-claim gap matters
  operationally. Unlikely to.

## 8. Risks

**Migration in a patch.** Additive nullable column; 046 shipped the same way
in 0.16.2. Pre-047 replicas ignore it. The backfill is idempotent.

**Two point queries per entry**, for every job whether or not it uses state.
Measured by the new histogram; a pre-check is the fallback.

**Collision regressions.** Rare names, all framework keys, surfaced at render
time by `collisions()`, listed in the release note, and closable at
validation.

**Merge surface.** `rendering.rs`, `job_creator.rs`, `worker_api/{jobs,
state}.rs`, `web/api/state_upload.rs`, `cascade.rs`,
`settlement/{mod,settle,dispatch}.rs`, both state repos, migration 047,
`metrics.rs`. Land as one change; rebase rather than merge.
