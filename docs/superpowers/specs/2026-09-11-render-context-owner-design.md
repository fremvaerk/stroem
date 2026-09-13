# One Owner for the Step Render Context — Design

Status: revision 5, proposed
Ships in: 0.16.4 (patch; additive migration 047; documented behaviour changes, §6)

Addresses candidate 4 of the 11 September 2026 architecture review, which ranked
it the top recommendation. Line numbers cite `main` at `f100d68` (spec-only
commits since `fba4e87`; code lines unchanged).

## Revision history

**Revision 5 (2026-09-13).** Revision 4's review raised eight findings, three
of them against the lazy backfill of legacy rows. All three stem from one
fact: snapshot storage keys are per **job** (`state_storage.rs:42`, `:57` —
`{prefix}{ws}/{task}/{job_id}.tar.gz`), not per snapshot, so a second upload
from the same job overwrites the tarball while inserting a second row. A
backfill keyed on the row could read another upload's bytes. Decision: **no
backfill.** Legacy rows keep today's archive fallback at claim time; at
cascade time they have no `state` until their next upload writes the column.
Also: the wording "move state to the DB" was wrong and is gone — the tarball
(the state *files*) stays in the archive and the worker path is untouched;
only a copy of the parsed `state.json` sidecar is persisted (§3.4). One
extractor for all four upload sites, root-preferred (§3.4); the six row-type
readers are enumerated (§3.4); "mixed fleet" is qualified by how `sqlx`
migrations actually behave (§3.4); collision reporting is `warn!` everywhere
and a job-log line only where a caller has async log access (§3.3); the
collision table is re-derived per key × scope × status (§6).

**Revision 4 (2026-09-12)** replaced per-advance archive fetches with a
persisted sidecar (migration 047) after the review showed `has_json` is an
uploader flag (`state.rs:135`, `:430`) and reading `state` means downloading,
gunzipping and walking the whole tarball (`extract_state_json`,
`state.rs:196-222`); and split `ContextInputs` into a per-entry `JobContext`
and per-render arguments because the cascade rebuilds its context from
mutated rows between phases (`cascade.rs:674`, `:687`).

**Revision 3 (2026-09-12)** withdrew revision 2's "two families": the cascade
can have state, because `execute` already does DB reads before calling the
pure `run` (`cascade.rs:890` precedes `pool.begin()` at `:895`). It also
showed the documented `when: "{{ not state or … }}"` guard never worked — the
`when` context has no `state` — and that `441a6ca` did not fix it.

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
| `secret` position | before steps | before | before | **after** steps, if nonempty | **after**, if nonempty | **after**, if nonempty |
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
6. A step named `secret` is shadowed by the secrets at cascade time (when the
   workspace has any) and shadows them at claim time; a step named `each` is
   always shadowed by the loop variable. Every framework key has a different
   collision story.
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
`claim_job`, and it must exist there because the parsed sidecar is only in
the tarball. Every other renderer is state-blind by location, not decision.

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
module is given. Snapshot resolution for rendering has one implementation,
needs only a pool, and is called from every entry that renders. Adding a
variable is a one-line change in one place.

**Non-goals.** The CLI builder (candidate 3). Hook and event-source
constructors (`settlement/hooks.rs:551`, `event_source.rs:356`). Per-instance
`when` (§4.2). The cross-workspace agent gap (§7). Connection secrets reached
via `{{ input.* }}` at cascade time (TODO.md, residual of `b21036b`).
Reserved-name validation (§7). Re-keying snapshot storage per snapshot (§7).
Moving state *files* anywhere: the tarball and the worker's `/state` mount are
untouched.

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
    collisions: Vec<Collision>,   // step names that shadowed a framework key
}
impl RenderContext {
    pub fn as_value(&self) -> &serde_json::Value;
    pub fn collisions(&self) -> &[Collision];
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
    pub id: Uuid,                        // the row
    pub storage_key: String,             // ClaimResponse; worker download
    pub has_json: bool,                  // the tarball carries a sidecar
    pub json: Option<serde_json::Value>, // the persisted copy, if any
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
   rendered input. A step whose sanitized name is one of the first five keys
   shadows it; `each` and approval's mapping shadow a step. This is today's
   majority behaviour per key (§1.1 "position" rows), chosen to change the
   fewest sites: the only order that flips is `secret` at S4–S6 (§6). `build`
   records every collision in `collisions()` and emits
   `tracing::warn!(job_id, step, key)`. **A job-log line is appended only
   where the caller has async log access** — `claim_job` — because `run` is
   synchronous and pure and its `Plan` is discarded by both callers
   (`settle.rs:167`, `dispatch.rs:546`); routing collisions out of the cascade
   to the job log is not worth a `Plan` field. Rejecting these names at
   validation is the class fix (§7).
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

### 3.4 The sidecar copy, and resolution

**What moves and what does not.** A snapshot is a tarball in the archive —
the files a step wrote to `/state-out`, plus a `state.json` sidecar when it
emitted `STATE:` lines. That tarball stays where it is; the worker still
downloads it by `storage_key` and mounts it at `/state`; nothing on that path
changes. Migration 047 adds a **copy** of the parsed sidecar to the
snapshot's row so the server can render `{{ state.x }}` without the archive.

**Migration 047.** `task_state` and `workspace_state` gain
`state_json JSONB NULL`. Written at every upload from bytes already in
memory, by one extractor:

| Site | Bytes | `state_json` |
|---|---|---|
| worker task upload, `state.rs:~423` | `body` | `extract_state_json(&body)` |
| worker global upload, `state.rs:~127` | `body` | same |
| API task upload, `state_upload.rs:511` | `repacked` | `extract_state_json(&repacked)` |
| API global upload, `state_upload.rs:~600` | `repacked` | same |

`extract_state_json` (`state.rs:196-222`) today returns the **first**
`state.json` at any depth in tar order. It becomes root-preferred: a
root-level `state.json` wins, else the first nested one. The API path
generates the root sidecar itself and sorts entries lexicographically
(`state_upload.rs:81`, `:95`, `:208`), so without this a tarball holding both
`a/state.json` and `state.json` would persist different JSON per path and
claim-time rendering would read the nested one. Behaviour change only for
tarballs with both (§6). `has_json` keeps its meaning — the uploader's
statement that the tarball carries a sidecar — and `state_json` may be NULL
when `has_json` is true (sidecar unparseable, or a legacy row); both simply
mean "no parsed state" for rendering.

`insert_and_prune` (both repos) gains `state_json: Option<serde_json::Value>`.
`TaskStateRow` / `WorkspaceStateRow` gain the field, and **all six** readers
project it: `get_latest`, `get`, `list` in `task_state.rs:29`, `:46`, `:153`
and `workspace_state.rs:25`, `:41`, `:146`.

**Rollout.** The column is additive and nullable: a running pre-047 replica
ignores it and keeps serving claims with the archive fallback. A pre-047
replica that *restarts* after 047 is applied fails startup — `sqlx::migrate!`
runs with defaults (`pool.rs:9`) and rejects an applied version missing from
the binary — which is how every migration in this repository has behaved
(046 shipped in 0.16.2 the same way). The Helm `RollingUpdate` with
`maxUnavailable: 0` and the PDB are what make that safe; noted in the release
note, not new.

**Legacy rows — no backfill.** A row with `state_json IS NULL` written before
047 is rendered exactly as today at claim time — archive fetch + extract,
`jobs.rs:533-606` — and contributes no `state` at cascade time until the
task's next upload writes the column. Revision 4 proposed writing the
extracted value back; that is unsafe because storage keys are per job
(`state_storage.rs:42`, `:57`) and a later upload from the same job replaces
the tarball under the same key (`state.rs:403`, `task_state.rs:110`;
`blob_storage.rs:168` for local), so a backfill could persist another
upload's sidecar into an older row. The cascade-time gap for legacy
snapshots is bounded by "one more run of the task" and costs nothing that
worked before — `when:` never saw state. Recorded in the release note.

**Resolution — pool only.**

```rust
// crates/stroem-server/src/render_context.rs
pub async fn latest_snapshots(pool: &PgPool, workspace: &str, task_name: &str) -> Snapshots
```

Two indexed point queries (`TaskStateRepo::get_latest`,
`WorkspaceStateRepo::get_latest`). No archive access. A lookup error is
logged at `warn` and yields `None` for that snapshot. A row is returned
whole, so `storage_key` and `has_json` reach `ClaimResponse` regardless of
`json`, and the worker keeps downloading after any server-side JSON problem
— as `jobs.rs:539-556` behaves today. `claim_job` alone keeps an archive
fallback: when `json` is `None` and `has_json` is true, it retrieves and
extracts for *rendering only*, never writing back.

**Entries.** Every entry that renders resolves once, at its start, and
threads `&Snapshots` down. `cascade::execute`, `run`, `handle_task_steps`,
`handle_approval_steps`, `fail_task_step` and
`orchestrate_after_server_step_failure` take `&Snapshots` and never resolve.

| Entry | Resolves | Threads to |
|---|---|---|
| `Settlement::advance` (non-terminal branch, `mod.rs:214`) | once | `cascade_and_settle` → `execute` → `run`; `handle_task_steps` → `fail_task_step` → `orchestrate_after_server_step_failure` → `cascade_and_settle`; `handle_approval_steps` |
| `dispatch::init` (`dispatch.rs:537`) | once — gains `task_name: &str` from the creator's `task_ref` (one call site, `job_creator.rs:486`) | `execute`, `handle_task_steps`, `handle_approval_steps` |
| `claim_job` | once, replacing `jobs.rs:533-606`, keeping the archive fallback | S1–S4 |

`Settlement` gains no field: the render path needs no `StateStorage`.

**What is guaranteed about freshness.** One sample per entry. A `when`
evaluated in an `advance` and the `script:` rendered at the subsequent claim
can see different snapshots if an upload lands between them — the existing
claim-time semantics extended one hop. Within an entry: guard-miss retries in
`execute` (`MAX_ATTEMPTS = 3`, `cascade.rs:862`) reuse the entry's sample;
nested advances reached through `reconcile` → child `advance` → `propagate`
→ parent `advance` (`mod.rs:447`, `propagate.rs:150`) each sample
independently, so an outer entry can finish on an older sample than an inner
one took. Rows are append-only and `get_latest` orders by `created_at, id`,
so "older" means "the previous row", never a torn read. Not a guarantee of
latest-at-render; stated so nobody relies on one.

**Cost.** Two point queries per entry for every job. Observed via a new
histogram `stroem_snapshot_resolve_seconds{entry}` (settlement is outside
the RED middleware, which covers `/api` only — `web/mod.rs:95`), documented
in `operations/metrics.md`. A "task references `state`" pre-check is the
fallback if it shows.

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
snapshot whose tarball has no `state.json`; a sidecar that did not parse; or
a legacy row at cascade time (§3.4). It is *not* a test for "no snapshot was
ever written" — a task that stores files but never emits `STATE:` always
takes that branch, which is the correct reading of "no structured state".
With that meaning the guide's expiry guard works — measured against
`evaluate_condition` with the context `build` produces:

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
2 promised is retired.

### 4.4 Placement

`stroem-server`. `build` is a pure function of plain data, so lifting it to
`stroem-common` for the CLI later (candidate 3) is a move. `latest_snapshots`
is server-only.

### 4.5 Secrets

`RenderContext` wraps `Secret<serde_json::Value>` per CLAUDE.md's "Secrets in
logs" rule. The persisted-error scrubs from `b21036b` are unaffected; the
connection-secret residual is unchanged. `state_json` holds whatever the step
wrote to `state.json` — it is job output, already visible through `/state`
on the worker and through `{{ state.* }}` at claim time; the column adds no
exposure that the tarball did not already have.

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
(integration, one per site). `extract_state_json` prefers a root
`state.json` over a nested one and still finds a nested-only one. Every one
of the six readers returns the column (one test per repo over `get_latest`,
`get`, `list`). `latest_snapshots`: no rows; a row without JSON; a row with
JSON; a lookup error yields `None` without failing. `claim_job` with a legacy
row: renders from the archive and does not write the column.

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
| tarball with both root and nested `state.json` | uploads, claim | first in tar order | root wins |
| legacy snapshot (pre-047 row), cascade time | S5, S6 | — | no `state` until the task's next upload |

**Collisions** — a step whose sanitized name equals a framework key. Cells
read *today → after*; "step" = the step entry wins, "key" = the framework
value wins, "undef" = neither exists. Derived from §1.1's position rows:
today S1–S3 include only completed rows; S4–S6 include all four statuses and
insert `secret` after steps only when the workspace has secrets.

| step named… | S1, S2 completed | S1, S2 skipped/failed/susp | S3 completed | S3 skipped/failed/susp | S4–S6 (any status) |
|---|---|---|---|---|---|
| `input` | step → step | **key → step** | step → step | **key → step** | step → step (approval mapping still wins) |
| `secret` | step → step | **key → step** | step → step | **key → step** | **key → step** if secrets nonempty; step → step if empty |
| `state`, `global_state` | step → step | **key → step** | step → step | undef → step | step → step |
| `job` | step → step | **key → step** | step → step | **key → step** | step → step |
| `each` | key → key | key → key | key → key | key → key | key → key (S5: step → step, `each` never present) |

Bold cells are regressions: at cascade time a step named `secret` now beats
a nonempty secrets map; at claim time a non-completed step named
`input`/`secret`/`state`/`global_state`/`job` — filtered out today
(`jobs.rs:524`) — now shadows the key. `undef → step` is additive. Every
regression is reported by `collisions()` (§3.3 rule 1) and in the release
note; §7 proposes rejecting the names.

¹ Instance names are `format!("{}[{}]", ..)` (`cascade.rs:605`); Tera parses
`{{ process[0] }}` as indexing into `process`, so the keys cannot be named,
but they are observable through `{{ __tera_context }}` (tera 1.20.1
`processor.rs:21`). No workflow in the repository uses either.

## 7. Follow-ons

- **Reserved step names** (`input`, `secret`, `state`, `global_state`, `job`,
  `each`): reject at validation; retires the collision table as a class.
  Belongs with candidate 2 — validation is not wired into server load today.
- **Key snapshots by snapshot id**, not job id, so two uploads from one job
  cannot overwrite each other's tarball. Pre-existing; also the root of the
  review's "template and mount can disagree" defect. Once done, a backfill of
  `state_json` for legacy rows becomes safe if ever wanted.
- **Availability validation** shrinks to one rule: `each` is unavailable in
  `when:`.
- **`AgentPrompt` input** could switch to prepared step input for symmetry
  with `ActionBody`. Behaviour change; own decision.
- **Cross-workspace agent steps** still render against the caller's config;
  MCP selection at `jobs.rs:795` and task-tool creation at `jobs.rs:1106`
  resolve against the caller independently. This design gives the prompt
  half one place to live.

## 8. Risks

**Migration in a patch.** Additive nullable column; 046 shipped the same way
in 0.16.2. Restart behaviour of pre-047 replicas is the standard one (§3.4).

**Two point queries per entry**, for every job whether or not it uses state.
Measured by the new histogram; a pre-check is the fallback.

**Collision regressions.** Rare names, all framework keys, surfaced by
`collisions()`, listed in the release note, closable at validation.

**Legacy snapshots are state-blind at cascade time until re-uploaded.** By
design (§3.4); costs nothing that worked before.

**Merge surface.** `rendering.rs`, `job_creator.rs`, `worker_api/{jobs,
state}.rs`, `web/api/state_upload.rs`, `cascade.rs`,
`settlement/{mod,settle,dispatch}.rs`, both state repos, migration 047,
`metrics.rs`. Land as one change; rebase rather than merge.
