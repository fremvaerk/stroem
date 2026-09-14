# One Owner for the Step Render Context — Design

Status: revision 10, proposed
Ships in: 0.16.4 (patch; additive migration 047; documented behaviour changes, §6)

Addresses candidate 4 of the 11 September 2026 architecture review, which ranked
it the top recommendation. Line numbers cite `main` at `9916985` (spec-only
commits since `fba4e87`; code lines unchanged).

## Revision history

**Revision 10 (2026-09-14, implementation).** Amendments made during
implementation, found by an adversarial post-implementation review: §3.3
rule 1's collision line is `tracing::debug!`, not `tracing::warn!` — the
cascade rebuilds the context twice per pass across up to three `execute`
attempts on every `advance`, so a `warn!` inside `build` would repeat for the
job's whole lifetime; the claim path re-emits each deduplicated line at
`warn` when it flushes to the job log. Two §6 behaviour-table rows were
added to match the shipped code rather than the original text: `{{
prev.output }}` on a completed step with NULL output now renders `""`
(landed in the earlier fix wave, `795d87b`); and claiming with no
`state_storage` configured but snapshot rows present still resolves `{{
state.* }}` / `{{ global_state.* }}` from the persisted sidecar, though the
claim response carries no snapshot keys (§6, this revision).

**Revision 9 (2026-09-13).** Revision 8's review closed three of four
findings and left one, exact: a `json` column is not enough on its own,
because sqlx 0.8.6 encodes `serde_json::Value` as a **JSONB parameter** on
the wire regardless of the target column's type
(`sqlx-postgres-0.8.6/src/types/json.rs:17`), so the NUL escape would be
rejected at bind time before the column is reached. The write path binds
the serialized sidecar as text and casts in SQL (`$N::json`); reads into
`Value` are unaffected (§3.4). A round-trip test with a NUL escape pins it
(§5).

**Revision 8 (2026-09-13).** Revision 7's review found the scope cut clean —
no citation stale, nothing remaining that depends on the storage design —
and one defect local to this document: PostgreSQL `jsonb` rejects `\u0000`
inside strings, while the extractor's `serde_json::from_str` (`state.rs:218`)
accepts it, so a sidecar containing a NUL escape — accepted by every upload
today — would fail the `INSERT` and trigger the upload's blob-deletion
compensation (`state.rs:437`). The column is `json`, not `jsonb`: PostgreSQL's
`json` type stores the text verbatim and accepts `\u0000`, and nothing here
uses `jsonb` operators — the value is only ever read whole (§3.4). The §4.6
race narratives are stated with the transaction ordering that actually
produces them, and the failed-upload compensation branch is added as (vi).

**Revision 7 (2026-09-13). Scope cut.** Revisions 4–6 each fixed the previous
review's race by pulling one more piece of task-state *storage* into this
design — per-job keys, pruning, backfill, re-keying, extractor semantics —
and each review found the next race one layer down. Revision 6's review
found six more, all in storage, and one is decisive: the worker does not
download the snapshot the claim names; it sends `(workspace, task)` and the
server picks latest again (`poller.rs:368`, `client.rs:619`, `state.rs:329`).
"Template and mount can disagree" is in the worker protocol — the
architecture review's red-band defect #2 — and closing it needs a wire
change, the very constraint this candidate was ranked first for not having.

Decision: **decouple.** This design persists the sidecar and builds the one
context; it changes no storage key, no extractor, no pruning, and adds no
backfill. Every storage race it inherits is enumerated with its interleaving
in §4.6 and owned by a second design,
`2026-09-13-task-state-storage-hardening-design.md`. Revision 6's re-keying
and backfill move there. Also: `task-state.md:103`; the creator's parameter
is `task_name` (`job_creator.rs:200`); `job_creator.rs:792-997`.

**Revision 6 (2026-09-13)** keyed uploads by snapshot id and added a
leader-gated backfill; both now belong to the storage design.
**Revision 5 (2026-09-13)** corrected "move state to the DB" to "persist a
copy of the sidecar" and dropped a claim-time backfill (per-job keys made it
unsafe). **Revision 4 (2026-09-12)** replaced per-advance archive fetches
with a persisted sidecar (migration 047) after the review showed `has_json`
is an uploader flag and reading `state` means downloading, gunzipping and
walking the tarball (`extract_state_json`, `state.rs:196-222`); and split
`ContextInputs` into a per-entry `JobContext` and per-render arguments.
**Revision 3 (2026-09-12)** withdrew "two families": the cascade can have
state, because `execute` already does DB reads before calling the pure `run`
(`cascade.rs:890` precedes `pool.begin()` at `:895`); and showed the
documented `when: "{{ not state or … }}"` guard never worked and `441a6ca`
did not fix it. **Revision 2 (2026-09-11)** corrected revision 1's premise
that undefined variables render empty (they error: `template.rs:82-84`,
`test_render_missing_variable:936`) and its promise of `each` in `when:`.
Two defects found during review shipped separately: `1c31db8` and `b21036b`.

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
"if present" = the key is inserted only when its value exists (job input
`Some`, secrets nonempty, a snapshot with JSON, a loop instance).

| context variable | S1 `input:` | S2 action body | S3 `image:` | S4 agent | S5 `when:` | S6 task-input / approval |
|---|:---:|:---:|:---:|:---:|:---:|:---:|
| `input` | job, if present | prepared step, if present | prepared step, if present | job, if present | job, if present | job / **job→step if nonempty** |
| `secret` value | caller ws | **owner** ws | **owner** ws | caller ws | caller ws | caller ws |
| `secret` position | before steps, if nonempty | before, always (`{}`) | before, always | **after** steps, if nonempty | **after**, if nonempty | **after**, if nonempty |
| `job` | before steps, always | before | before | before | before | before |
| statuses included | completed | completed | completed | c+s+f+susp | c+s+f+susp | c+s+f+susp |
| `.error` on failed | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ |
| `state`/`global_state` | if present | if present | **✗ err** | **✗ err** | **✗ err** | **✗ err** |
| `each` | after steps, if loop | after, if loop | after, if loop | after, if loop *(`1c31db8`)* | **never** | after, if loop |
| approval's rendered `input` | — | — | — | — | — | after steps |
| loop-instance rows | included | included | included | skipped | skipped | skipped |

### 1.2 The author-visible bugs

1. `{{ state.x }}` in an `image:` fails the step. S3 has no state parameter
   (`rendering.rs:355-363`) although the caller holds both values.
2. `{{ state.x }}` in an agent prompt fails the step. S4's builder has no
   state parameter at all.
3. `{{ state.x }}` in a `when:` is always undefined, so the documented expiry
   guard `when: "{{ not state or state.days_remaining < 30 }}"`
   (`task-state.md:103`) runs the step on every pass. The feature the guide
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

**Non-goals.** Task-state storage: keys, pruning, the worker download
protocol, extractor policy, backfill of legacy rows — all owned by
`2026-09-13-task-state-storage-hardening-design.md` (§4.6). The CLI builder
(candidate 3). Hook and event-source constructors (`settlement/hooks.rs:551`,
`event_source.rs:356`). Per-instance `when` (§4.2). The cross-workspace agent
gap (§7). Connection secrets reached via `{{ input.* }}` at cascade time
(TODO.md, residual of `b21036b`). Reserved-name validation (§7). Moving state
*files* anywhere: the tarball and the worker's `/state` mount are untouched.

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
    pub storage_key: String,             // ClaimResponse
    pub has_json: bool,                  // uploader's flag: the tarball carries a sidecar
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
   `tracing::debug!(job_id, step, key)` — `debug`, not `warn`, because the
   cascade rebuilds the context twice per pass across up to three `execute`
   attempts on every `advance`, so a warn inside `build` would repeat for the
   job's whole lifetime. The claim path, which flushes the lines to the job
   log, re-emits each deduplicated line at `warn`. A job-log line is appended
   only where the caller has async log access — `claim_job` — because `run`
   is synchronous and pure and its `Plan` is discarded by both callers
   (`settle.rs:167`, `dispatch.rs:546`). Rejecting these names at validation
   is the class fix (§7).
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
downloads and mounts it at `/state`; nothing on that path changes. Migration
047 adds a **copy** of the parsed sidecar to the snapshot's row so the server
can render `{{ state.x }}` without the archive. Storage keys, pruning, the
extractor and the worker protocol are **unchanged** — see §4.6 for what that
inherits.

**Migration 047.** `task_state` and `workspace_state` gain
`state_json JSON NULL` — `json`, not `jsonb`: `jsonb` rejects `\u0000` in
strings, `serde_json` accepts it (`state.rs:218`), and a sidecar with a NUL
escape is a valid upload today; `json` stores the text verbatim, and the
column is only ever read whole, never queried by key. Written at every upload
from bytes already in memory, using the **existing** `extract_state_json` (`state.rs:196-222`)
gated on the uploader's flag exactly as the claim path gates it today
(`jobs.rs:539`):

| Site | Bytes | `state_json` |
|---|---|---|
| worker task upload, `state.rs:~423` | `body` | `if query.has_json { extract_state_json(&body) } else { None }` |
| worker global upload, `state.rs:~128` | `body` | same |
| API task upload, `state_upload.rs:511` | `repacked` | same, with the flag from `:117` |
| API global upload, `state_upload.rs:719` | `repacked` | same |

So the persisted value is byte-for-byte what today's claim-time extraction
would compute for that tarball — same extractor, same gate — moved from
every claim to once per upload. `has_json` keeps its meaning. `state_json`
may be NULL with `has_json` true (unparseable sidecar; or a row written
before 047 or by a pre-047 replica), which rendering treats as "no parsed
state".

`insert_and_prune` (both repos) gains `state_json: Option<serde_json::Value>`
and **binds it as text with an SQL cast** — `serde_json::to_string(&v)` bound
as `Option<String>`, and `$N::json` in the `INSERT` — not as a `Value`. sqlx
0.8.6 encodes `serde_json::Value` through `Json<T>`, whose parameter type is
JSONB (`sqlx-postgres-0.8.6/src/types/json.rs:17`) and is sent at statement
preparation whatever the column is; a NUL escape would be rejected at bind
time, and the upload's compensation would then delete the blob it had just
stored (`state.rs:439`). Text-plus-cast makes Postgres parse it as `json`,
which accepts the escape. Reading the column back into
`Option<serde_json::Value>` is unaffected: the decode path accepts both
`json` and `jsonb` and hands the bytes to `serde_json::from_slice`.
`TaskStateRow` / `WorkspaceStateRow` gain the field, and **all six** readers
project it: `get_latest`, `get`, `list` in `task_state.rs:29`, `:46`, `:153`
and `workspace_state.rs:25`, `:41`, `:146`.

**Rows without the column.** Written before 047, or by a pre-047 replica
during the rolling update, or with an unparseable sidecar. At claim time
they render exactly as today: archive fetch + extract when `has_json`
(`jobs.rs:533-606`), for rendering only, never written back. At cascade
time they contribute no `state` until a later upload writes a row with the
column. No backfill in this design: a backfill is only safe once no writer
can overwrite a per-job key, which is the storage design's first change
(§4.6). Cost of the gap: nothing that worked before — `when:` never saw
state — plus one trap, release-noted: a newly written state-dependent
`when:` on a task whose only snapshots predate 047 sees no `state` until
that task uploads again.

**Rollout.** The column is additive and nullable: a running pre-047 replica
ignores it and keeps serving claims with the archive fallback. A pre-047
replica that *restarts* after 047 is applied fails startup — `sqlx::migrate!`
runs with defaults (`pool.rs:18`) and rejects an applied version missing
from the binary — as every migration in this repository has behaved (046
shipped in 0.16.2 the same way). The Helm `RollingUpdate` with
`maxUnavailable: 0` and the PDB are what make that safe; release-noted.

**Resolution — pool only.**

```rust
// crates/stroem-server/src/render_context.rs
pub async fn latest_snapshots(pool: &PgPool, workspace: &str, task_name: &str) -> Snapshots
```

Two indexed point queries (`TaskStateRepo::get_latest`,
`WorkspaceStateRepo::get_latest`). No archive access. A lookup error is
logged at `warn` and yields `None` for that snapshot. A row is returned
whole, so `storage_key` and `has_json` reach `ClaimResponse` regardless of
`json` — as `jobs.rs:539-556` behaves today.

**Entries.** Every entry that renders resolves once, at its start, and
threads `&Snapshots` down. `cascade::execute`, `run`, `handle_task_steps`,
`handle_approval_steps`, `fail_task_step` and
`orchestrate_after_server_step_failure` take `&Snapshots` and never resolve.

| Entry | Resolves | Threads to |
|---|---|---|
| `Settlement::advance` (non-terminal branch, `mod.rs:214`) | once | `cascade_and_settle` → `execute` → `run`; `handle_task_steps` → `fail_task_step` → `orchestrate_after_server_step_failure` → `cascade_and_settle`; `handle_approval_steps` |
| `dispatch::init` (`dispatch.rs:537`) | once — gains `task_name: &str`, which the creator already holds (`job_creator.rs:200`; one call site, `:486`) | `execute`, `handle_task_steps`, `handle_approval_steps` |
| `claim_job` | once, replacing `jobs.rs:533-606`, keeping the archive fallback for rows without the column | S1–S4 |

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
so "older" means "the previous row". Not a guarantee of latest-at-render, and
not a guarantee that the rendered `json` and the worker's mount are the same
upload — that is §4.6 (ii) and (iii), inherited.

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
`job_creator.rs:792-997`, which migrate to the new module; `job_context`
moves there. The three secret scrubs from `b21036b` (`jobs.rs:378`,
`cascade.rs:703`, `dispatch.rs:75`) act on error strings after rendering and
are untouched.

## 4. Decisions

### 4.1 Presence semantics for state, and the guide's example

`state` is inserted only when `Snapshot.json` is `Some`. `not state` is
therefore true when **no parsed sidecar is available**: no snapshot; a
snapshot uploaded without `has_json`; a sidecar that did not parse; or, at
cascade time, a row without the column (§3.4). It is *not* a test for "no
snapshot was ever written" — a task that stores files but never emits
`STATE:` always takes that branch, which is the correct reading of "no
structured state". With that meaning the guide's expiry guard works —
measured against `evaluate_condition` with the context `build` produces:

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

No availability axis exists, so nothing enforces one.

### 4.4 Placement

`stroem-server`. `build` is a pure function of plain data, so lifting it to
`stroem-common` for the CLI later (candidate 3) is a move. `latest_snapshots`
is server-only.

### 4.5 Secrets

`RenderContext` wraps `Secret<serde_json::Value>` per CLAUDE.md's "Secrets in
logs" rule. The persisted-error scrubs from `b21036b` are unaffected; the
connection-secret residual is unchanged. `state_json` holds whatever the step
wrote to `state.json` — job output, already visible through `/state` on the
worker and through `{{ state.* }}` at claim time; the column adds no exposure
the tarball did not already have.

### 4.6 Known, inherited, not fixed here

Task-state storage has consistency defects that predate this design. Each is
stated with its interleaving so the boundary is exact. All are owned by
`2026-09-13-task-state-storage-hardening-design.md`.

**(i) Per-job keys; the same blob is overwritten by later uploads from the
same job.** Key = `{prefix}{ws}/{task}/{job_id}.tar.gz`
(`state_storage.rs:42-49`; global `:57-62`); the blob is stored before the
row is inserted (`state.rs:404` → `:423`). Sequential steps of one job that
each write state produce N rows sharing one blob. *Effect of this design:*
the persisted `json` on each row is the sidecar of *that* upload, while the
shared blob holds the *last* one. The schedule that makes them disagree:
A stores its blob under K and pauses **before** `pool.begin()`
(`state.rs:404` → `:413`); B stores under K, begins, inserts, commits; A
begins — its `created_at` is transaction-start `NOW()`
(`028_task_state.sql:9`), later than B's — inserts, commits. `get_latest`
orders by `created_at, id` (`task_state.rs:33`), so A is latest with A's
`json`, and K holds B. Today claim-time rendering reads K and so renders B;
with the column it renders A. Whether that is visible to the worker is (ii).

**(ii) The worker does not download the snapshot the claim named.** It
checks `state_storage_key` is present, then requests `(workspace, task)`
(`poller.rs:368`, `client.rs:619`) and the server selects latest again
(`state.rs:329`). Any upload between claim and download changes what is
mounted. This is the review's red-band defect "template and mount can
disagree", and it means the rendered `json` and the mounted files are not
guaranteed to be the same upload with or without this design. Fixing it is a
wire change (download by the claim-supplied key).

**(iii) Pruning deletes a shared blob.** `insert_and_prune` returns pruned
rows' keys (`task_state.rs:127-141`) and the handler deletes each
(`state.rs:459`). With (i), pruning an older same-job row deletes the blob
the newer row still points at. Data loss today, independent of this design.

**(iv) Extractor semantics.** `extract_state_json` returns the *first*
`state.json` at any depth in tar order (`state.rs:215`); the API path sorts
entries lexicographically (`state_upload.rs:208`) so `a/state.json` precedes
a root one; duplicate roots take the first, whereas the worker's
`Archive::unpack` (`poller.rs:55`) lets the last overwrite. This design
persists exactly what claim-time extraction computes today, so it neither
fixes nor changes these; it makes the computed value durable.

**(v) Rows without the column** (§3.4) cannot be backfilled safely while (i)
holds, because a backfill could persist a later upload's sidecar into an
earlier row.

**(vi) A failed upload's compensation deletes a blob a retained row still
references.** The handler stores the blob first, then deletes it if
beginning the transaction, inserting, or committing fails (`state.rs:416`,
`:439`, `:453`). With (i), that blob is K, shared with the retained rows of
the same job: a retained row is left with no blob at all. Independent of
pruning, so a surviving-reference check on prune does not cover it.

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
equal to what `extract_state_json` returns for the same bytes, and NULL when
the flag is false (integration, one per site). **A sidecar containing a NUL
escape** (`{"cursor":"a\u0000b"}`) round-trips through `insert_and_prune`
and `get_latest` unchanged — this is the test that fails if anyone binds the
value as `serde_json::Value` again. Each of the six readers
returns the column. `latest_snapshots`: no rows; a row without JSON; a row
with JSON; a lookup error yields `None` without failing. `claim_job` with a
row lacking the column renders from the archive and does not write back.

**Wiring tests — one per entry, the lesson of `441a6ca`.** A snapshot value
must reach the rendered field on the production path from each entry:
`advance` (`{{ state.x }}` in a `when:` on a step promoted after a
completion), `init` (`{{ state.x }}` in a root step's `when:` — the guide's
`check-expiry` shape), `claim_job` (`{{ state.x }}` in an `image:` and an
agent prompt), and the failure path (`{{ state.x }}` in a `when:` evaluated
after a `type: task` dispatch failure). Each verified by reverting the wiring
and observing the failure. `integration_test.rs:3000` stays.

**Existing coverage.** The 53 `rendering.rs` tests migrate; net count falls as
triplets collapse. `state.rs:293`'s nested-extraction test is untouched.

## 6. Behaviour changes

Not all additive.

| Change | Sites | Today | After |
|---|---|---|---|
| `{{ state.x }}` in `image:`, agent prompt | S3, S4 | step fails | resolves |
| `{{ state.x }}` in `when:`, `for_each:` | S5 | undefined → `not state` always true | resolves; the guide's guard works |
| `{{ state.x }}` in task-step input, approval message | S6 | step fails | resolves |
| `{{ failed.error }}` in `input:`/action body | S1–S3 | step fails | resolves |
| skipped/failed/suspended refs in `input:`/action body | S1–S3 | step fails | `.output` renders `""` (`null` through Tera; falsy in `when:`) |
| `{{ prev.output }}`, `prev` completed with NULL output | S1–S6 | step fails (undefined) | `""` |
| `{% if secret is defined %}`, no workspace secrets | S1, S4–S6 | false | true |
| loop-instance entries at claim | S1–S3 | present | absent¹ |
| approval `{{ input.foo }}` with no step mapping | S6 | job input | job input (unchanged, now specified) |
| row without the column, cascade time | S5, S6 | — | no `state` until the task uploads again |
| same-job overwrite race (§4.6 i), claim-time render | S1–S4 | blob's (last) sidecar | latest row's own sidecar |
| claim with no state_storage configured but snapshot rows present | S1–S4 | state undefined (lookup skipped) | resolves from the persisted sidecar; response carries no snapshot keys |

**Collisions** — a step whose sanitized name equals a framework key.

The rule: a cell is a **regression** only if the framework key is *present*
today in that scope (so the template resolves to it) and the step entry will
displace it. Where the key is absent today the template errors, and
resolving to the step is additive (`undef → step`). Presence today, from
§1.1: `input` when the scope's input is `Some`; `secret` when nonempty
(S1, S4–S6) or always (S2, S3); `state`/`global_state` when a snapshot has
JSON (S1, S2 only); `job` always; `each` only for a loop instance.

| step named… | S1–S3, completed | S1–S3, skipped/failed/suspended | S4–S6, any status |
|---|---|---|---|
| `input` | step → step | **key → step** if input present, else undef → step | step → step (approval mapping still wins) |
| `secret` | step → step | **key → step** (S2, S3 always; S1 if nonempty), else undef → step | **key → step** if nonempty, else step → step |
| `state`, `global_state` | step → step | **key → step** in S1, S2 if a snapshot has JSON; else undef → step (S3 always) | step → step |
| `job` | step → step | **key → step** | step → step |
| `each` | key → key if loop instance, else step → step | key → key if loop instance, else undef → step | key → key if loop instance, else step → step (S5: always step) |

Bold cells are the regressions: at cascade time a step named `secret` now
beats a nonempty secrets map; at claim time a non-completed step named
`input`/`secret`/`state`/`global_state`/`job` — filtered out today
(`jobs.rs:524`) — now shadows a key that is present. Every regression is
reported by `collisions()` (§3.3 rule 1) and in the release note; §7 proposes
rejecting the names.

¹ Instance names are `format!("{}[{}]", ..)` (`cascade.rs:605`); Tera parses
`{{ process[0] }}` as indexing into `process`, so the keys cannot be named,
but they are observable through `{{ __tera_context }}` (tera 1.20.1
`processor.rs:21`). No workflow in the repository uses either.

### Release notes for 0.16.4

1. A completed step that produced no output now renders `{{ step.output }}`
   as an empty string instead of failing the step (all template fields).
2. Agent `prompt`/`system_prompt` rendered when the job's workspace config is
   unavailable now see the full context (previously rendered against `{}` and
   failed on any variable).
3. Flow steps named `input`, `secret`, `state`, `global_state`, `job` or
   `each` now shadow / are shadowed uniformly in every field (see the
   collision table above); the server logs a `[render] … shadows …` line.
4. Task-state snapshots written before migration 047 are not visible to
   `when:` / `for_each:` / `type: task` inputs / approval messages until the
   task uploads a new snapshot (claim-time rendering is unaffected).

## 7. Follow-ons

- **Task-state storage hardening** —
  `2026-09-13-task-state-storage-hardening-design.md`: per-snapshot keys,
  download by claim-supplied key, prune with a surviving-reference check,
  extractor policy, and then a safe backfill of rows without the column.
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

## 8. Risks

**Migration in a patch.** Additive nullable column; 046 shipped the same way
in 0.16.2. Restart behaviour of pre-047 replicas is the standard one (§3.4).

**Two point queries per entry**, for every job whether or not it uses state.
Measured by the new histogram; a pre-check is the fallback.

**Inherited storage races become durable** (§4.6 i). The same-job overwrite
race already exists at the worker boundary (ii); this design adds one more
place its effect can be seen, and records it rather than hides it. The
storage design closes the class.

**Collision regressions.** Rare names, all framework keys, surfaced by
`collisions()`, listed in the release note, closable at validation.

**Merge surface.** `rendering.rs`, `job_creator.rs`, `worker_api/{jobs,
state}.rs`, `web/api/state_upload.rs`, `cascade.rs`,
`settlement/{mod,settle,dispatch}.rs`, both state repos, migration 047,
`metrics.rs`. Land as one change; rebase rather than merge.
