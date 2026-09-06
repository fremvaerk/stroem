# Cross-Workspace Connections — Design

**Status:** Draft (awaiting review)
**Date:** 2026-09-06
**Builds on:** `2026-07-29-cross-workspace-references-design.md` (cross-workspace actions)

## 1. Problem

Cross-workspace *actions* shipped in v0.15.23: a flow step may say
`action: jobs.recalc-agg-sessions` and the step runs in the `jobs` workspace's
context, including that workspace's connections. That is the **only** way today
to use another workspace's connection. The guide's "Not yet supported" list
carries the gaps this design closes:

- A **qualified connection reference** (`jobs.clickhouse-prod`) used anywhere
  other than the connection-typed input of a `jobs`-owned action is rejected
  with *"connection 'jobs.clickhouse-prod' does not exist"*. The resolver
  (`stroem_common::template::resolve_connection_inputs`) does a plain map lookup
  on a single `WorkspaceConfig` and never parses the dot.
- A **qualified connection-type reference** (`type: jobs.clickhouse` on a task or
  action input, or `type: jobs.clickhouse` on a connection) fails offline
  validation (`validate_connection_inputs`) and is unusable.
- The task-detail endpoint builds the UI's connection dropdown from the caller
  workspace's connections only.
- Job-detail redaction (`web/api/jobs.rs::redact_response`) masks only the
  caller workspace's secret values. An owner workspace's secrets that reach a
  persisted step input/output are returned in plain text. This gap already
  exists for cross-workspace actions; direct connection references would make
  it easy to hit.

A workspace author should be able to write

```yaml
tasks:
  daily:
    input:
      clickhouse: { type: jobs.clickhouse, default: jobs.clickhouse-prod }
    flow:
      run:
        action: score-all          # LOCAL action
        input:
          clickhouse: "{{ input.clickhouse }}"
```

with no local ClickHouse type, connection, or secret — and the owner of
`clickhouse-prod` must have opted in to that.

## 2. Goals / Non-Goals

### Goals

1. A connection-typed input (task-level or action-level) accepts a **qualified
   connection name** `ws.conn`, resolved server-side against workspace `ws`.
2. A connection-type reference — on an input's `type:` **and** on a connection's
   own `type:` — may be **qualified** (`ws.type_name`).
3. Types and connections are addressed **independently**: the type may live in
   one workspace and the connection in another, as long as the connection
   declares that same (qualified) type.
4. A per-connection **`shared: true`** flag gates every cross-workspace use of a
   connection. Default `false`. No workspace-level `exports:` list.
5. The task-detail endpoint lists eligible shared foreign connections in the
   dropdown, by qualified name. The UI is unchanged.
6. Job-detail redaction covers the secrets of every workspace a job references.
7. Offline (`stroem validate` / `stroem run`) behaviour degrades with clear
   messages rather than false errors.

### Non-Goals (explicitly deferred)

- Workspace-level `exports:` / allow-list — rejected for now; `shared` is
  per-connection and global.
- A `shared` flag on connection **types**. Types are schemas, not secrets.
- Cross-workspace `type: task` actions, hook actions, agent steps — unchanged
  from the cross-workspace-references design.
- Pinning the owner's revision for a *connection* value. The value is captured
  at resolution time (job creation for task inputs, claim time for action
  inputs), which is the existing behaviour for local connections.
- ACL on cross-workspace reads beyond the `shared` flag.

## 3. Reference model

### 3.1 Canonical type identity

Every connection-type reference resolves to a canonical pair
`(workspace, type_name)`:

| Written as                       | In workspace | Canonical            |
|----------------------------------|--------------|----------------------|
| `type: clickhouse`               | `A`          | `(A, clickhouse)`    |
| `type: jobs.clickhouse`          | anywhere     | `(jobs, clickhouse)` |
| `type: common.clickhouse` (lib)  | `A`          | `(A, common.clickhouse)` — see 3.3 |

A **bare** name means "the workspace this YAML is written in". Two references
match **only when their canonical pairs are equal**. This is symmetric and
needs no special cases: two workspaces that both define a type named
`clickhouse` are distinct types, and a connection can only satisfy an input if
both point at the same definition.

The same canonicalisation applies to a connection's own `type:` field.

### 3.2 Worked examples

Given workspace `jobs` defines type `clickhouse` and workspace `infra` defines
connection `ch-eu` with `type: jobs.clickhouse, shared: true`:

| Caller input `type:`  | Value                | Result |
|-----------------------|----------------------|--------|
| `jobs.clickhouse`     | `jobs.clickhouse-prod` | OK if `clickhouse-prod` in `jobs` is `shared` and its type is `clickhouse` (bare in `jobs` ⇒ `(jobs, clickhouse)`) |
| `jobs.clickhouse`     | `infra.ch-eu`        | OK — `ch-eu` declares `jobs.clickhouse` ⇒ `(jobs, clickhouse)`, and is shared |
| `clickhouse` (local)  | `jobs.clickhouse-prod` | **Reject**: `(caller, clickhouse) ≠ (jobs, clickhouse)` |
| `jobs.clickhouse`     | `jobs.private-ch`    | **Reject**: exists but `shared: false` |
| `jobs.clickhouse`     | `nope.x`             | **Reject**: unknown workspace `nope` |
| `jobs.clickhouse`     | `local-ch` (bare)    | OK only if the caller's local `local-ch` declares `type: jobs.clickhouse` |

### 3.3 Precedence: library first, then workspace

Same rule as actions (`job_creator.rs`, cross-workspace-references §4.2):

1. A dotted name is first tried **literally** as a key in the local workspace
   config. Libraries flatten their *connection types* under `libname.type`, so
   `common.clickhouse` is a literal local key and canonicalises to
   `(caller, common.clickhouse)`.
2. On a local miss, split on the **first** `.` into `(workspace, item)`. If
   `workspace` is a loaded workspace, look `item` up there.
3. Unqualified names are always local. Fully backward-compatible.

Connections are **never** library-imported (libraries ignore `connections:`),
so for connection names step 1 is only for consistency and never matches a
library item today.

`stroem_common::template::parse_qualified_ref` already implements the split.

### 3.4 The `shared` flag

```rust
pub struct ConnectionDef {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub connection_type: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shared: bool,                  // NEW
    #[serde(flatten)]
    pub values: HashMap<String, serde_json::Value>,
}
```

`shared` is a reserved key, so it must be pulled out of the flattened `values`
map exactly like `type` already is. A connection with a value literally named
`shared` today would silently change meaning — acceptable; document it.

Semantics:

- Referenced by **bare name from its own workspace**: `shared` is ignored.
- Referenced by **qualified name from another workspace**: must be `shared`,
  else resolution fails with a distinct error
  *"connection 'jobs.private-ch' exists but is not shared"*.
- **Cross-workspace actions** resolving their own connection-typed inputs by
  bare name in the owner context (existing behaviour) are the owner reading its
  own config: **ungated**, unchanged.
- Untyped connections (no `type:`) skip the type check today and satisfy any
  connection-typed input. That rule is kept unchanged for foreign references:
  an untyped `shared: true` connection matches any input type. The owner opted
  in explicitly, and the values are passed verbatim exactly as for a local
  untyped connection. Document: "prefer typed shared connections".

## 4. Resolution: where and how

All resolution stays **server-side**. The worker never sees a connection name;
it receives the already-expanded values object in the step's input JSON, exactly
as today. A cross-workspace connection therefore adds **no** tarball download —
the step still downloads exactly one workspace (its action's owner).

### 4.1 New lookup abstraction in `stroem-common`

```rust
/// Resolves a workspace name to its config. Implemented by the server over the
/// WorkspaceManager (pre-collected, sync) and by the CLI over the single local
/// config.
pub trait WorkspaceLookup {
    /// The workspace the caller's YAML lives in.
    fn local(&self) -> &WorkspaceConfig;
    fn local_name(&self) -> &str;
    /// Any other loaded workspace, or None if unknown / not loaded.
    fn get(&self, name: &str) -> Option<&WorkspaceConfig>;
}
```

`resolve_connection_inputs(input, schema, workspace_config)` becomes
`resolve_connection_inputs(input, schema, &impl WorkspaceLookup)`;
`prepare_action_input` follows. A `SingleWorkspace<'a>(&'a str, &'a WorkspaceConfig)`
adapter keeps every existing call site and test compiling with a one-line change.

Inside the resolver, for each non-primitive input field:

1. Canonicalise the **field type** via §3.3 (literal-local first, then split).
   Unknown workspace / unknown type ⇒ error naming the field and the type.
2. Read the value. Object ⇒ pass through (existing inline escape hatch, unchanged).
   Non-string ⇒ existing error.
3. Resolve the **connection name** via §3.3. Unknown ⇒ existing
   *"references connection … which does not exist"* error, now naming the
   workspace searched. Found in a foreign workspace and `!shared` ⇒ new
   *"exists but is not shared"* error.
4. Canonicalise the connection's declared type **in the connection's own
   workspace** and compare pairs. Mismatch ⇒ existing type-mismatch error,
   messages now print canonical `ws.type` on both sides.
5. Replace the string with `conn.values`.

### 4.2 Server: pre-collecting configs

`WorkspaceManager::get_config` is async and returns `Arc<WorkspaceConfig>`;
the resolver is sync and runs inside CPU-bound render paths. The server builds a
`WorkspaceSet` — `HashMap<String, Arc<WorkspaceConfig>>` + local name — before
calling the resolver:

- Collect every workspace prefix that appears in (a) the task's input types and
  default values, (b) the action's input types and defaults, (c) the submitted
  input string values, (d) any connection `type:` in already-collected configs
  (one extra hop: a shared connection in `infra` may declare `type: jobs.x`).
  Two hops are sufficient because a connection's type is a leaf; loop until the
  set stops growing, bounded by the number of loaded workspaces.
- `get_config` each; a name that is not a loaded workspace is simply absent from
  the set and the resolver reports it as unknown.

`WorkspaceSet` implements `WorkspaceLookup`. A helper
`job_creator::collect_workspace_set(workspaces, local_name, local_cfg, &[&json])`
is the single place this scan lives.

### 4.3 Call sites

| Site | File | Change |
|------|------|--------|
| Task input at job creation | `job_creator.rs` ~L172 | build `WorkspaceSet` from task schema + effective input; pass it |
| Action input at job creation | `job_creator.rs` ~L492 | same, from action schema + rendered input |
| Action input at claim time | `web/worker_api/rendering.rs::prepare_step_action_input` ~L188 | `RenderContext` gains `workspace_set: &WorkspaceSet` (built in `worker_api/jobs.rs::claim_job` next to the existing owner-config fetch); local = `action_ws` for cross-workspace steps, caller otherwise |
| CLI `stroem run` | `stroem-cli/src/local/run.rs` L52, L359 | `SingleWorkspace`; any dotted name that is not a literal local key ⇒ error *"cross-workspace connection references require a server; run this task via `stroem-api trigger`"* |

For the claim-time site the "local" workspace is the **action owner** (the
existing `action_ws` choice) so that bare names in a cross-workspace action's
defaults keep resolving in the owner, and a qualified name in the caller's
flow-step `input:` map resolves globally. Both work with one lookup.

### 4.4 Error classification

`web/api/tasks.rs::is_user_error` already maps *"resolve connection"* and
*"does not exist"* to 400. Add *"is not shared"* and *"unknown workspace"* so
every author mistake is a 400, never a 500. An owner workspace that is loaded
but currently unhealthy (config unavailable) stays a 500, matching actions.

## 5. Validation

`stroem_common::validation`:

- `validate_connection_inputs` (task + action input types) and the connection
  `type:` check in `validate_connections`: a dotted type that is not a literal
  local key is **skipped with a warning** (*"'jobs.clickhouse' is a
  cross-workspace reference; validated at job creation"*), mirroring the
  existing dotted-action treatment when `libraries_resolved == false`.
- When `libraries_resolved == true` (server-side path) the same skip applies —
  there is no cross-workspace-aware validation entry point today (pre-existing
  gap noted in CLAUDE.md), and job creation remains the safety net.
- A connection whose type is dotted is never checked at load time (offline
  validation skips it, and there is no cross-workspace-aware load-time
  validator). So **only for dotted-typed connections**, the resolver validates
  the connection's values against the owner's `ConnectionTypeDef` when it is
  resolved (§4.1 step 4 already has the type def; reuse the property check from
  `validate_connections` factored into a shared fn). Failure ⇒ 400. Local-typed
  connections were already validated at load and are not re-checked.
- A `default:` on a connection-typed input may be a qualified name. Offline
  validation skips checking that the default exists when it is dotted.

## 6. Task-detail dropdown

`web/api/tasks.rs::get_task` builds `connections: HashMap<type_string, Vec<name>>`
keyed by the input's `type:` string **as written** (the UI looks it up by the
field's own type). New logic:

1. For each non-primitive input type, canonicalise (§3.3) using a `WorkspaceSet`
   built from all loaded workspaces (`WorkspaceManager::names()` + `get_config`
   each; in-memory `Arc` clones, no I/O).
2. Scan **every** loaded workspace's connections. Include a connection when its
   canonical type equals the input's canonical type **and** (it is local **or**
   it is `shared`).
3. Emit local matches by bare name, foreign matches as `ws.name`. Sort local
   first, then foreign alphabetically.

The UI (`ui/src/pages/task-detail` form) already renders a `<select>` from this
map and submits the chosen string. No change. Re-run prefill stores the raw
string, so `jobs.clickhouse-prod` replays unchanged.

Discovery note: because the endpoint requires `View` on the task and only lists
`shared` foreign connections, the dropdown cannot expose a non-shared
connection's *name* from another workspace.

## 7. Redaction

`web/api/jobs.rs::get_job` currently does
`collect_secret_values(&caller_ws.secrets)`. Change to union the secrets of:

- the job's own workspace,
- every distinct `job_step.action_workspace` for the job (already stored, one
  query the handler performs anyway to list steps),
- every workspace prefix found in string values of `job.raw_input` and the task's
  input defaults (a `jobs.clickhouse-prod` submission ⇒ include `jobs`), plus the
  one-hop expansion from §4.2 for connections whose type is foreign.

Both `job.input` and each step's `input` are then redacted against the union.
This also closes the pre-existing exposure for cross-workspace actions. Cost:
a few extra `Arc` clones per job-detail request.

`raw_input` redaction limitation (values not present in any workspace's
`secrets` are never masked) is unchanged and remains documented.

## 8. Worker

No change. The worker crate contains no connection handling; it receives
resolved values inline. `ClaimResponse.workspace`/`revision` keep pointing at
the action's owner. One tarball per step, as before.

## 9. Files touched

**stroem-common**
- `models/workflow.rs` — `ConnectionDef.shared`; custom `Deserialize` (or a
  `#[serde(flatten)]`-compatible helper) that pulls `shared` out of the value map.
- `template.rs` — `WorkspaceLookup`, `SingleWorkspace`, `canonical_type_ref`,
  `resolve_connection_ref`, resolver rewrite, error messages.
- `validation.rs` — dotted-type skip with warning; factor the property-schema
  check into a reusable fn.

**stroem-server**
- `job_creator.rs` — `WorkspaceSet` + `collect_workspace_set`; two call sites.
- `web/worker_api/rendering.rs`, `web/worker_api/jobs.rs` — claim-time set on
  `RenderContext`.
- `web/api/tasks.rs` — dropdown; `is_user_error` additions.
- `web/api/jobs.rs` — redaction union.

**stroem-cli**
- `local/run.rs`, `local/validate.rs` — `SingleWorkspace`, clear errors/warnings.

**Docs**
- `docs/src/content/docs/guides/cross-workspace-references.md` — new
  "Connections" section; shrink "Not yet supported".
- `docs/src/content/docs/guides/connections.md` — `shared`, qualified `type:`.
- `CLAUDE.md` §Cross-Workspace References + §Connections.
- `docs/internal/TODO.md` — tick the deferred items.
- `docs/public/llms.txt` regenerates from the above.

**UI** — none.

## 10. Testing

**stroem-common unit tests** (`template.rs`, `validation.rs`, `models`):
- `shared` (de)serialises; absent ⇒ `false`; not leaked into `values`; a
  value key literally named `shared` is consumed as the flag (documented).
- `canonical_type_ref`: bare, dotted, library-literal precedence, unknown ws.
- Resolver, every row of §3.2, plus: local bare unchanged; inline object
  pass-through unchanged; foreign untyped shared ⇒ matches any type (existing
  rule); foreign dotted-typed connection with values failing the owner's
  schema ⇒ error.
- `SingleWorkspace` rejects dotted names with the "requires a server" message.
- Validation: dotted input type ⇒ warning not error; dotted connection type ⇒
  warning; literal library key ⇒ no warning; default dotted ⇒ skipped.

**stroem-server integration tests** (`tests/`, testcontainers Postgres, two
folder workspaces `caller` + `owner` + a third `infra` for the two-hop case):
- Job creation with `owner.shared-conn` ⇒ 201, `job.input` holds the values.
- Same with `owner.private-conn` ⇒ 400 "not shared".
- `infra.ch-eu` declaring `type: owner.clickhouse` ⇒ 201 (two-hop).
- Caller-local `type: clickhouse` + `owner.shared-conn` ⇒ 400 mismatch.
- Claim path: local action, flow-step `input: {ch: "owner.shared-conn"}` ⇒
  claimed step input contains the values; `ClaimResponse.workspace == caller`.
- Claim path: cross-workspace action with bare-name default in owner, owner's
  connection `shared: false` ⇒ still resolves (ungated owner self-reference).
- Task detail: dropdown lists `shared-conn` as `owner.shared-conn`, omits
  `private-conn`, lists local first.
- Job detail: owner's secret value appearing in `job.input` / step input is
  masked to `••••••`.
- Regression: existing cross-workspace action tests untouched and green.

**stroem-cli**: `stroem validate` on a workspace using `owner.type` prints the
warning and exits 0; `stroem run` with a dotted connection exits non-zero with
the "requires a server" message.

**E2E** (`tests/e2e.sh`): one scenario — two workspaces, caller task with
`type: owner.clickhouse` input defaulting to `owner.clickhouse-prod`, local
script action echoes `{{ input.clickhouse.host }}`; assert the job completes
and the log contains the owner's host value.

## 11. Rollout / compatibility

- Additive. No DB migration: nothing new is persisted; qualified names already
  fit in the existing JSONB inputs.
- Existing YAML is unaffected unless a connection had a value key named
  `shared` (now consumed as the flag; release note).
- Existing cross-workspace *action* behaviour is byte-for-byte unchanged for
  bare-name owner connections.

## 12. Open questions (answered during brainstorming, recorded here)

- **Type and connection from different workspaces?** Yes — independent dotted
  addressing, matched by canonical pair (§3.1).
- **Gate?** Per-connection `shared: true`; no `exports:` list.
- **Scan all workspaces for the dropdown?** Yes — required because a matching
  shared connection may live in any workspace; in-memory only.
- **Bare local type + foreign connection?** Rejected by the canonical rule; a
  local connection may *opt in* by declaring the foreign type instead.
