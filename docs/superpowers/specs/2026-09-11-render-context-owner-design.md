# One Owner for the Step Render Context — Design

Status: revision 2, proposed
Ships in: 0.16.4 (patch; carries documented behaviour changes, §6)

Addresses candidate 4 of the 11 September 2026 architecture review, which ranked
it the top recommendation.

## Revision 2 (2026-09-11)

Revision 1 was reviewed and found unsafe to implement. It inherited a premise
from the architecture review that does not hold, and three of its promises were
structurally impossible. Everything below is re-derived from the code and each
claim is cited. The corrections, because they change the shape of the work:

1. **Undefined variables do not render empty — they error.** `render_template`
   uses a default `Tera` and propagates the error (`template.rs:82-84`);
   `test_render_missing_variable` (`template.rs:936`) asserts `is_err()`. The
   review's framing of "silent divergence" is wrong: `{{ state.x }}` in an
   `image:` does not produce an empty image, it **fails the step**. Revision 1's
   whole behaviour-change analysis was built on the wrong direction.
2. **`state` is not uniformly available, and cannot be.** `cascade::run` is a
   synchronous pure function over rows (`cascade.rs:649-654`), while a state
   snapshot needs a DB read plus an archive fetch, reachable only through
   `AppState.state_storage` in `claim_job`. Revision 1's "same rule everywhere"
   would have required making the cascade async and I/O-bound — undoing the
   purity that shipped as review candidate 1.
3. **`each` can never reach a `when`.** The placeholder's condition is evaluated
   at `cascade.rs:544`, *before* `parse_for_each_items` at `:563`, and instances
   are created with `when_condition: None` (`cascade.rs:617`). Revision 1
   promised this and it is not deliverable.
4. **There are six call sites, not four.** Revision 1 missed approval-message
   rendering (`dispatch.rs:289-330`) and `type: task` child input rendering
   (`dispatch.rs:118-150`).

Two defects identified during that review are already fixed and shipped in
`1c31db8`, ahead of this design: agent `prompt`/`system_prompt` render failures
are no longer swallowed, and the agent render context now receives `each`. §5
records what that leaves.

## 1. Problem

"What can an author reference in a template here?" has six answers and no owner.
The same variable resolves — or hard-fails — differently depending on which YAML
field it sits in.

| # | Call site | Renders | Runs in |
|---|---|---|---|
| S1 | `rendering.rs:47-128` `render_step_input` | flow step `input:` | `claim_job` |
| S2 | `rendering.rs:211-349` `render_action_spec` | `env`/`cmd`/`script`/`source`/`manifest`/`args` | `claim_job` |
| S3 | `rendering.rs:355-405` `render_image` | `image:` | `claim_job` |
| S4 | `jobs.rs:740` → `build_step_render_context` | agent `prompt`/`system_prompt` | `claim_job` |
| S5 | `cascade.rs:678,687` → same | `when:` | `cascade::run` (pure) |
| S6 | `dispatch.rs:118`, `:289` → same | `type: task` step `input:`, approval `message:` | settlement |

A seventh builder, `stroem-cli/src/local/run.rs:434`, belongs to review
candidate 3 and is out of scope; it is named because it constrains placement
(§4.4).

### 1.1 The two families

The six sites split by *what data the call path can supply*, which is the fact
revision 1 missed:

**Claim-time** (S1–S4) runs inside `claim_job` (`jobs.rs:400-884`). It has the
pool, the archive, the resolved step input, and the task/global state snapshots
(`state_json_value` at `:523`, `global_state_json_value` at `:563`).

**Cascade-time** (S5–S6) runs inside `cascade::run` — pure, synchronous, no DB —
or inside settlement, within the apply transaction. Neither has archive access,
and giving the cascade any would destroy the property candidate 1 shipped.

This boundary is real and must be modelled, not erased. Everything *else* about
the six is accidental.

### 1.2 The divergence matrix

Verified by reading each site. "err" = the template hard-fails the step.

| context variable | S1 `input:` | S2 action body | S3 `image:` | S4 agent | S5 `when:` | S6 task-input / approval |
|---|:---:|:---:|:---:|:---:|:---:|:---:|
| `input` | job | rendered step | rendered step | job | job | job / **job→rendered (2-phase)** |
| `secret` | caller ws | **owner** ws | **owner** ws | caller ws | caller ws | caller ws |
| `job` | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| statuses included | completed | completed | completed | c+s+f+susp | c+s+f+susp | c+s+f+susp |
| `.error` on failed | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ |
| `state`/`global_state` | ✓ | ✓ | **✗ err** | **✗ err** | n/a | n/a |
| `each` | ✓ | ✓ | ✓ | ✓ *(fixed, `1c31db8`)* | **impossible** | ✓ |
| loop-instance rows | included | included | included | skipped | skipped | skipped |

Two rows that revision 1 got wrong and are **not** divergences:

- **`output` on a completed step with NULL output.** All six omit the key
  (`rendering.rs:105-107`, `job_creator.rs:625-627`), so `{{ prev.output }}`
  errors everywhere alike. Revision 1 claimed S5/S6 returned `null`; they do so
  only for skipped/failed/suspended steps, which S1–S3 never include at all.
- **`secret` presence.** S1/S4–S6 omit the key when the map is empty, S2/S3
  insert `{}`. Since `{{ secret.foo }}` errors either way — an absent key and a
  missing key on an empty object are both Tera errors — this is invisible to
  authors. Not worth a rule.

### 1.3 The author-visible bugs that remain

1. **`{{ state.x }}` / `{{ global_state.x }}` in an `image:` fails the step.**
   S3 has no state parameter (`rendering.rs:355-363` — eight parameters, neither
   of them state) even though the caller has both values in scope 60 lines
   earlier. Claim-time; closable.
2. **Same in an agent `prompt`/`system_prompt`.** S4 calls
   `build_step_render_context`, which has no state parameter at all. Claim-time;
   closable.
3. **`{{ state.x }}` in a flow step's `input:` works for a worker step and fails
   for a `type: task` step.** Identical YAML field, different call path: S1 has
   state, S6 does not. This is the sharpest one, and it is the family boundary
   made visible — not closable without a decision (§4.3).
4. **`{{ failed_step.error }}` works in `when:` but fails in a step `input:` or
   action body.** S1–S3 include only completed steps.
5. **Loop-instance rows** are in the context at claim time and absent at
   cascade time.

### 1.4 Why the shape produces them

**Hand-threaded parameters.** S2 takes ten positional arguments and S3 takes
eight — seven the same values, threaded by hand at `jobs.rs:671-703`. Bug 1 is
not something someone wrote; it is a parameter that does not exist in that
signature. `#[allow(clippy::too_many_arguments)]` at `rendering.rs:210` marks
the spot.

**An open return type.** `build_step_render_context` returns
`serde_json::Value`, so callers can patch variables in afterwards —
`dispatch.rs:120-131` and `:291-302` add `each`, and `jobs.rs:740` did not
(bug 2's sibling, fixed in `1c31db8`). The missing owner and the permissive
return type are the same defect.

### 1.5 Why the tests did not catch it

53 unit tests sit on S1–S3 in `rendering.rs` and none covers the wiring between
them. The pattern is a hand-maintained triplet asserting one property once per
builder — `test_render_{step_input,action_spec,image}_step_named_job_shadows_job_metadata`
(`:2266`, `:2293`, `:2316`), and the same for `job.revision` at `:2169`,
`:2223`, `:2245`. The state triplet is **incomplete**:
`test_render_step_input_with_state_json` (`:1890`) has no `render_action_spec`
or `render_image` counterpart. That missing third test is bug 1.

Failures that are caught need Postgres: `integration_test.rs:2668` and `:3000`
exist purely to guard argument threading.

## 2. Goals and non-goals

**Goals.** One module owns context construction. The two axes that are real
(`which input`, `whose secrets`) become data it is given. The accidental
differences collapse. Adding a variable becomes a one-line change in one place,
and omitting one becomes a compile error rather than a failed step.

**Non-goals.** The CLI builder (candidate 3). The hook and event-source
constructors (`hooks.rs:549`, `event_source.rs:356`) — different shapes, not
step rendering. Making the cascade impure to obtain state (§4.3). Fixing the
cross-workspace agent gap (§7). The secret-leak defect in TODO.md, which is
orthogonal and pre-existing.

## 3. Design

### 3.1 The module

New: `crates/stroem-server/src/render_context.rs`.

```rust
/// What the call path can supply. Assembled once; never patched afterwards.
pub struct ContextInputs<'a> {
    pub job_input: Option<&'a serde_json::Value>,
    pub rendered_step_input: Option<&'a serde_json::Value>,
    pub caller_secrets: &'a HashMap<String, serde_json::Value>,
    pub owner_secrets: Option<&'a HashMap<String, serde_json::Value>>,
    pub steps: &'a [StepView],
    pub snapshots: Snapshots<'a>,
    pub loop_slot: Option<LoopSlot<'a>>,
    pub job_revision: Option<&'a str>,
}

/// The family boundary, as a type. `Unavailable` is not "no snapshot exists";
/// it is "this call path cannot read one" — see §4.3.
pub enum Snapshots<'a> {
    Available { state: Option<&'a serde_json::Value>,
                global_state: Option<&'a serde_json::Value> },
    Unavailable,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Scope { StepInput, ActionBody, AgentPrompt, Condition, ChildTaskInput, ApprovalMessage }

impl Scope {
    pub const ALL: [Scope; 6] = [ /* … */ ];
    /// Claim-time scopes must be given `Snapshots::Available`.
    pub fn is_claim_time(self) -> bool;
}

/// Opaque: the only constructor is `build`, so no caller can patch a variable
/// in after the fact. Carries rendered secrets, so it is `Secret`-wrapped and
/// has no `Debug` that prints contents (CLAUDE.md, "Secrets in logs").
pub struct RenderContext(Secret<serde_json::Value>);

impl RenderContext {
    pub fn as_value(&self) -> &serde_json::Value;
}

pub fn build(inputs: &ContextInputs, scope: Scope) -> RenderContext;
```

`StepView` is a projection of the five fields the context needs — `step_name`,
`status`, `output`, `error_message`, `loop_source` — so the module is
unit-testable without a database and does not depend on `JobStepRow`'s shape.

### 3.2 The scope-dependent surface

This table is the whole `match scope`.

| `Scope` | `input` | `secret` | family |
|---|---|---|---|
| `StepInput` | job | caller | claim |
| `ActionBody` | rendered step | **owner** | claim |
| `AgentPrompt` | job | caller | claim |
| `Condition` | job | caller | cascade |
| `ChildTaskInput` | job | caller | cascade |
| `ApprovalMessage` | rendered step | caller | cascade |

`ActionBody` covers S2 and S3, which already agree on both axes — collapsing
them is what closes bug 1.

**`secret` is a security boundary, not a hole.** Action bodies resolve against
the OWNER workspace's secrets, step inputs against the CALLER's
(`jobs.rs:658-668` vs `rendering.rs:75-79`). Giving step-input rendering the
owner's secrets would let a caller exfiltrate a foreign workspace's secrets by
templating them into an input — cross-workspace actions are open while
connections are `shared`-gated precisely to stop that. This axis stays.

**`input` cannot be unified.** In `StepInput` the rendered step input is the
value being computed. `ApprovalMessage` is the two-phase case: `dispatch.rs:305-330`
builds a context, renders the step's flow `input:` against it, then substitutes
the result and renders the message. Under this design that is two `build` calls
— `ChildTaskInput` then `ApprovalMessage` — not a mutation of one context.
Revision 1 mapped approval to `Condition` and would have broken
`{{ input.changelog }}` in every approval message.

### 3.3 The unconditional rules

Identical in all six scopes:

1. `job` always inserted, **before** step outputs, so a step named `job` shadows
   it (preserves the CLAUDE.md compatibility rule; the ordering requirement
   moves from six places to one).
2. Step entries for **completed, skipped, failed and suspended**.
3. `output` present on every entry, `null` when the step produced none — this
   changes `{{ prev.output }}` on a null-output step from a hard failure to
   `null`, uniformly.
4. `error` on failed steps.
5. `each` inserted whenever the step is a loop instance and the scope can have
   one (`Condition` never can — §4.2).
6. Loop-instance rows skipped; only the placeholder's rolled-up aggregate
   appears, under the base step name.
7. `secret` always inserted, even when empty.
8. `state`/`global_state` inserted when `Snapshots::Available` carries them,
   omitted when `None`. **Presence, not always-insert** — see §4.1.

### 3.4 Call-site changes

S1–S3 lose their context assembly and their parameter lists: S2 goes from ten
positional parameters to two, S3 from eight to two, and the
`#[allow(clippy::too_many_arguments)]` is deleted rather than moved.
`claim_job` assembles one `ContextInputs` and calls `build` three times.
`cascade.rs:678,687` and `dispatch.rs:118,289` call `build` and **delete** their
post-hoc `each` patching, as does the agent path added in `1c31db8`.
`build_step_render_context` is deleted; `job_context` moves into the module.

`prepare_step_action_input` (`rendering.rs:139-208`) is explicitly **not**
absorbed. It resolves action defaults and connection provenance, not template
variables, and the regression at `integration_test.rs:2668` guards *it*, not
context assembly. That integration test stays exactly as it is.

## 4. Decisions

### 4.1 Presence, not always-insert, for state

Always-inserting an empty `state` would change `when: "not state"`-style
conditions, so presence semantics are preserved.

While verifying this, the documented example turned out to be broken.
`evaluate_condition` (`template.rs:338-346`) renders the string and tests
truthiness, so `when: "not state or state.days_remaining < 30"` — containing no
`{{ }}` — rendered to itself and was **always true**, making the condition a
silent no-op. Measured:

| form | no state | fresh (60d) | stale (10d) |
|---|---|---|---|
| bare, as documented | true | **true** | true |
| `{{ … }}` braced | true | false | true |

Fixed in `CLAUDE.md` and `docs/.../task-state.md` ahead of this design; the
braced form also does not error when no snapshot exists, which is what makes
presence semantics safe here.

### 4.2 `each` in `Condition` is not deliverable

`cascade.rs:544` evaluates the placeholder's `when` before `:563` parses the
collection, and instances carry `when_condition: None` (`:617`). `Scope::Condition`
therefore never receives a `LoopSlot`, and the module documents this as a
property of the execution model rather than a gap. Per-instance conditions would
need their own design.

### 4.3 The family boundary is modelled, not closed

Bug 3 — `{{ state.x }}` working in a worker step's `input:` and failing in a
`type: task` step's — is the one divergence this design does not remove. Closing
it means giving settlement archive access, which drags state acquisition into
the cascade's transaction and reopens the purity question. `Snapshots::Unavailable`
makes the boundary explicit and greppable instead of implicit, and the error
message names it ("task state is not available when rendering `<field>`") rather
than surfacing a bare Tera undefined-variable error. Closing it properly is a
follow-on with its own design.

### 4.4 Placement

`stroem-server`, not `stroem-common`. The review ranked this candidate first
partly for being contained and server-side; moving it to `stroem-common` to
share with the CLI front-runs candidate 3. `build` is a pure function of plain
data with no `AppState`, pool or I/O, so lifting it later is a move, not a
rewrite.

### 4.5 Secrets

`RenderContext` wraps `Secret<serde_json::Value>` and derives no `Debug` that
prints contents, per CLAUDE.md's "Secrets in logs" rule: the context contains
rendered secret values by construction. This does **not** address the separate,
pre-existing leak where Tera embeds the offending value in filter errors that
are then persisted (TODO.md, Security) — that is fixed at `fail_claimed_step`
and is orthogonal to this design.

## 5. Testing

**The anti-regression test.** One table-driven test over `Scope::ALL` asserting
every unconditional variable is present in every scope, with the two documented
exemptions (`state` under `Snapshots::Unavailable`, `each` under `Condition`)
named explicitly rather than skipped silently. It replaces the three
hand-maintained triplets of §1.5 and would have caught bug 1 when it was
introduced.

**Per-rule unit tests** for §3.3, each asserted once instead of once per
builder. **Scope-axis tests** for §3.2, including the two-phase approval flow.

**Wiring tests.** `integration_test.rs:3000` (`job.revision` threading) keeps
working and stays. Following the lesson from `1c31db8` — where unit tests on a
helper could not detect the wiring being reverted — each new claim-time scope
gets one integration assertion that the context actually reaches the rendered
field, verified by reverting the wiring and observing the failure.

## 6. Behaviour changes

Not "all additive" — revision 1 claimed that and it is false.

| Change | Today | After | Direction |
|---|---|---|---|
| `{{ state.x }}` in `image:` | step fails | resolves | fixes a failure |
| `{{ state.x }}` in agent prompt | step fails | resolves | fixes a failure |
| `{{ failed.error }}` in `input:`/action body | step fails | resolves | fixes a failure |
| `{{ prev.output }}`, completed, NULL output | step fails | `null` | fixes a failure |
| skipped/failed/suspended refs in `input:`/action body | step fails | `null` output | fixes a failure |
| loop-instance entries at claim (`process[0]`) | present | absent | unreachable either way¹ |
| **a skipped/failed step named `job`, in `input:`/action body** | **`{{ job.revision }}` works** | **shadowed; breaks** | **regression²** |

¹ Instance names are `format!("{}[{}]", ..)` (`cascade.rs:605`) and Tera parses
`{{ process[0] }}` as indexing into `process`, so those keys cannot be
referenced. Verified: no template in the repository references one.

² Rule 2 admits non-completed steps into claim-time scopes, so a *skipped* step
named `job` now shadows the job metadata where it previously did not. The
shadowing rule itself is pre-existing and deliberate; this widens which statuses
trigger it. Requires a release note. A step named `job` is already discouraged.

## 7. What this unblocks

Cross-workspace agent steps render `prompt`/`system_prompt` against the caller's
config rather than the owner's (CLAUDE.md, "Deferred"). This design does not fix
it and — correcting revision 1, which claimed it became a one-row change — it is
not a single edit: MCP server selection (`jobs.rs:773`) and task-tool child
creation (`jobs.rs:1077`) each resolve against the caller independently. What
the design does deliver is one place where the decision lives for the prompt
half, instead of a seventh divergence to introduce by hand.

The follow-on the review noted still holds: `validation.rs` syntax-checks `when`
(`:196`), `for_each` (`:225`), `prompt` (`:1602`) and `system_prompt` (`:1616`)
against a deliberately empty context, so it catches syntax errors but never an
unavailable variable. Once `Scope` declares what exists where, validation can
reject `{{ state.x }}` in an `image:` at author time. That belongs with
candidate 2.

## 8. Risks

**The family boundary may not hold.** If a future requirement needs state in a
`when`, `Snapshots` becomes a lie and the design needs revisiting. That is the
correct place for the pressure to show up.

**`ContextInputs` is wide** — eight fields, the shape the review criticises
elsewhere. It is a parameter object consumed by one function in one module, not
an interface many modules construct. Growth past what one `build` justifies is a
signal the scopes have genuinely diverged, not a signal to add a ninth
caller-supplied decision.

**Merge surface.** Touches `rendering.rs`, `job_creator.rs`, `jobs.rs`,
`cascade.rs` and `settlement/dispatch.rs`, four of which the cascade and
settlement work moved recently. Land as one change; rebase rather than merge.
