# Re-run Prefill & Job Lineage — Design

**Status:** Approved, ready for implementation planning
**Date:** 2026-04-28
**Scope:** Phase 4+ enhancement to the UI Re-run flow.

## Problem

Today, clicking **Re-run** on a job navigates to the task detail page with an empty form
(populated only from the task's `input` schema defaults). The user has to re-enter every
non-default value, including connection selections and secret overrides. For tasks with
many inputs, this turns Re-run into a copy-paste chore.

A naive prefill from `job.input` doesn't work because:

- The server resolves connection-type inputs to their full values object before persisting,
  so the original connection name is lost.
- Secret values are redacted to `••••••` in API responses, so the UI can't see (and
  shouldn't see) the original secret.
- Defaulted fields are merged in at job creation, so the persisted input doesn't tell us
  which values the user explicitly chose vs. which came from the schema default.

This design solves all three by persisting the user's submission verbatim (with secrets
redacted on the API boundary), and giving the server a way to replay non-default secrets
on behalf of the UI without ever exposing them.

## Non-Goals

- **Restart from a failed step (feature B).** Mentioned for forward-compat; the lineage
  fields land in this spec but the executor and UI behavior for `source_type = 'restart'`
  are deferred.
- **Per-step input editing on Re-run.** A user editing one step's rendered input on
  restart is not in scope. The existing `job_step.input` (rendered post-claim) is the
  starting point if/when that feature is built.
- **Bit-identical replay.** Re-run uses the current workspace revision, not the source
  job's revision. Reproducing an exact past run is a separate feature.

## Approach Summary

Persist `raw_input` (user's pre-resolution submission) on the `job` table. Add a
`source_job_id` lineage pointer plus extended `source_type` values (`'rerun'`,
`'restart'`). When a Re-run executes, the UI sends `source_job_id` plus form values; for
secret/connection fields the user didn't touch, the UI submits the redaction sentinel
`••••••` and the server replaces it with the source job's original value before
proceeding through the normal merge/resolve pipeline.

## Section 1 — Data Model

### `job` table changes (migration `032_job_raw_input_and_lineage.sql`)

```sql
ALTER TABLE job
  ADD COLUMN raw_input JSONB,
  ADD COLUMN source_job_id UUID REFERENCES job(job_id) ON DELETE SET NULL,
  ADD COLUMN restart_from_step TEXT;

-- Extend source_type CHECK to accept 'rerun' and 'restart'.
ALTER TABLE job DROP CONSTRAINT job_source_type_check;
ALTER TABLE job ADD CONSTRAINT job_source_type_check
  CHECK (source_type IN (
    'trigger', 'user', 'api', 'webhook', 'hook', 'task',
    'retry', 'mcp', 'upload', 'event_source',
    'rerun', 'restart'
  ));

CREATE INDEX idx_job_source_job_id ON job(source_job_id) WHERE source_job_id IS NOT NULL;
```

### Field semantics

| Column                | Type   | Nullable | Notes |
|-----------------------|--------|----------|-------|
| `raw_input`           | JSONB  | yes      | Verbatim user submission. Connection fields hold connection names. Omitted fields stay omitted (not filled with defaults). NULL for legacy rows. |
| `source_job_id`       | UUID   | yes      | Lineage pointer for `'rerun'` and `'restart'` source types. NULL for all other source types. |
| `restart_from_step`   | TEXT   | yes      | Set only when `source_type = 'restart'` (feature B). NULL in feature A. |

### Lineage model

| `source_type` | Tracking field          | Meaning                                                |
|---------------|-------------------------|--------------------------------------------------------|
| `retry`       | `retry_of_job_id`       | Auto-retry of a failed job (existing behavior)         |
| `rerun`       | `source_job_id`         | User clicked **Re-run** (feature A)                    |
| `restart`     | `source_job_id` + `restart_from_step` | User clicked **Restart from step** (feature B) |

`retry_of_job_id` is intentionally separate from `source_job_id`. Retry is the same
logical run, attempt N+1 (server-initiated). Re-run/restart are user-initiated new runs
with potentially different input. Conflating them muddies the audit trail.

### Backfill

None. Old jobs get `raw_input = NULL`. The UI handles `NULL` as "predates prefill" — see
Section 4.

## Section 2 — Server-Side Input Flow

### Before today

In `crates/stroem-server/src/job_creator.rs::create_job_for_task_inner`:

```text
incoming_input
  → merge_defaults(incoming_input, schema, secrets_ctx)
  → merged_input
  → resolve_connection_inputs(merged_input, schema, workspace_config)
  → resolved_input
  → JobRepo::create_with_parent_tx_id(input = Some(resolved_input))
```

### After this change

```text
incoming_input
  ↓ if source_job_id present:
  ↓   incoming_input := resolve_rerun_sentinels(incoming_input, source.raw_input, schema)
  ↓
  ↓ raw_input := incoming_input.clone()       ← persisted to job.raw_input
  ↓
  ↓ merge_defaults(...)
  ↓ resolve_connection_inputs(...)
  ↓ resolved_input                            ← persisted to job.input (existing)
```

### Two sentinels — clarifying terminology

The implementation uses two distinct sentinel strings; do not conflate them:

| Constant            | Value      | Where defined                                     | Meaning                                                        |
|---------------------|------------|---------------------------------------------------|----------------------------------------------------------------|
| `REDACTED`          | `••••••` (6× U+2022) | `crates/stroem-server/src/web/api/jobs.rs` | Server-side redaction marker in API responses (existing).      |
| `SECRET_SENTINEL`   | `********` (8× `*`) | `ui/src/components/task/constants.ts`     | UI-internal form placeholder meaning "field is unchanged from its stored default" (existing). |
| `REDACTED_SENTINEL` | `••••••` (6× U+2022) | NEW — `ui/src/components/task/constants.ts` | UI-side mirror of the server `REDACTED`, used to detect redacted values in `raw_input` and as the wire value sent to the server when replaying. Must equal `REDACTED` byte-for-byte. |

The two-sentinel split matters because the existing form already uses `SECRET_SENTINEL`
to mean "leave the secret alone, server should use the schema default" (today's
behavior). Re-run adds a third state — "leave the secret alone, server should replay
from the source job" — which we encode on the wire as `REDACTED_SENTINEL`.

### `resolve_rerun_sentinels`

New helper, alongside `merge_defaults` in `crates/stroem-common/src/template.rs`.

For each field in the task's input schema:

- If the field is **secret** or **a connection type**, AND `incoming_input[field]` equals
  the `REDACTED` sentinel (`••••••`):
  - If `source.raw_input[field]` exists → copy that value into `incoming_input[field]`.
  - Else → remove the field from `incoming_input` (so `merge_defaults` will fill it with
    the schema default — this supports secret rotation: if the source used the schema
    default and the underlying secret has been rotated, the new run picks up the new
    value).
- All other fields: leave as-is.

The UI never sends `••••••` for fields the user actually edited; it sends the new value
or an empty string. So the sentinel is unambiguous: "reuse from source."

### Authorization

`source_job_id` validation in the `/execute` handler:

1. Source job exists and belongs to the same `workspace` as the new job → else `400`.
2. Source job's `raw_input IS NOT NULL` → else `400` ("source job predates Re-run").
3. Requesting principal has **View** permission on the source job (same ACL gate as
   `GET /api/jobs/{id}`) → else `403`.

Without check #3, a low-privilege user with Run on a task could harvest secrets from
another user's jobs by guessing UUIDs.

### Storage path for non-Re-run jobs

Webhook, trigger, scheduler, task-action, hook, retry, MCP, upload, event-source paths
all store `raw_input = incoming_input` (the input they synthesized for that path) and
leave `source_job_id` NULL. This is mostly future-proofing — it means a webhook-driven
job can be Re-run later if we expose that, with no extra plumbing.

## Section 3 — API Shape

### `GET /api/jobs/{id}` response

New fields on `JobDetailResponse`:

```json
{
  "job_id": "...",
  "input": { ... },
  "raw_input": { ... } | null,
  "source_job_id": "..." | null,
  "restart_from_step": null,
  ...
}
```

`raw_input` is redacted using the existing `redact_json` pass: same `••••••` sentinel
for secret field values, same `ref+` substring redaction. Connection-type field values
in `raw_input` are connection names (strings), which are not sensitive and not redacted.

**Known redaction limitation (pre-existing, not introduced by this feature):** the
`redact_response` function matches only values that appear in `workspace.secrets`. A
user-typed secret value not present in the workspace config (e.g., a personal API key
typed into a `secret: true` field) is stored and returned as plain text. This same
gap already exists for `job.input` today — adding `raw_input` does not widen the
exposure surface, but it doubles the number of places the same string is returned.
A future hardening pass should make redaction field-aware (redact every value of a
field whose schema is `secret: true`, regardless of `workspace.secrets` membership).

### `POST /api/workspaces/{ws}/tasks/{task}/execute` request body

```json
{
  "input": { ... },
  "source_job_id": "..." | undefined
}
```

`source_job_id` is optional. Backwards compatible — existing clients (CLI, MCP,
non-Re-run UI flows) ignore the field and get today's behavior. The same body shape is
accepted by `/api/workspaces/{ws}/tasks/{task}/rerun/{source_job_id}`-style URLs?
**No** — we deliberately do not introduce a parallel endpoint. Source-job validation,
ACL checks, and sentinel resolution all live in the existing handler.

### TypeScript types (`ui/src/lib/types.ts`)

```ts
export interface JobDetail {
  // ... existing fields
  raw_input: Record<string, unknown> | null;
  source_job_id: string | null;
  restart_from_step: string | null;
}

// executeTask signature gets an optional opts object.
export function executeTask(
  workspace: string,
  taskId: string,
  input: Record<string, unknown>,
  opts?: { sourceJobId?: string },
): Promise<ExecuteTaskResponse>;
```

## Section 4 — UI Behavior

### Re-run button (`ui/src/pages/job-detail.tsx`)

```tsx
<Link
  to={`/workspaces/${ws}/tasks/${task}`}
  state={{ sourceJobId: job.job_id, rawInput: job.raw_input }}
>
  Re-run
</Link>
```

No additional fetch — the data is already on the page.

### Task detail prefill (`ui/src/pages/task-detail.tsx`)

Read `location.state` once at mount. Build the form's initial values with this
precedence (highest first):

1. `rawInput[key]` if set → use verbatim.
2. Else if `field.secret && field.default !== undefined` → `SECRET_SENTINEL`.
3. Else if `field.default !== undefined` → `field.default`.
4. Else if `field.type === "boolean"` → `false`.
5. Else → `""`.

After applying, call `window.history.replaceState(null, "", path)` so a page refresh
discards the prefill.

### Per-field-type rendering

The prefill loop sets the form's *internal* state value. The two sentinels (defined in
Section 2) flow as follows:

| Field type             | `rawInput[key]` value           | Form's internal value                | Rendered UI                            |
|------------------------|---------------------------------|--------------------------------------|----------------------------------------|
| Plain primitive        | actual value                    | actual value                         | Field populated with value             |
| Connection type        | connection name (string)        | connection name                      | Combobox selects that name             |
| Secret, source custom  | `REDACTED_SENTINEL` (`••••••`)  | `REDACTED_SENTINEL`                  | Password input + helper text "Using value from previous run — type to override" |
| Secret, source default | absent from `rawInput`          | `SECRET_SENTINEL` (today's behavior) | Password input, no helper text         |

Helper text only renders when the form's internal value is `REDACTED_SENTINEL`. This
satisfies the "support secrets if not equal to default" requirement: the user sees the
indicator exactly when prefill is replaying a custom value.

### Submit logic — extending `handleSubmit`

Today's loop strips `SECRET_SENTINEL` for secret fields. The new behavior also passes
`REDACTED_SENTINEL` through to the server when in re-run mode:

| User action on secret field | Form value      | UI sends                      | Server interprets             |
|-----------------------------|-----------------|-------------------------------|-------------------------------|
| Untouched (default sentinel)| `SECRET_SENTINEL` | nothing (stripped, today's behavior) | Use schema default        |
| Untouched (rerun sentinel)  | `REDACTED_SENTINEL` | `••••••`                  | Replay from `source.raw_input` |
| Edited (typed value)        | typed string    | typed string                  | Stored verbatim               |
| Cleared (empty input)       | `""`            | `""`                          | Stored verbatim (explicit empty) |

For non-secret fields (including connection-typed), the form value submits unchanged. A
user could in principle type the literal string `••••••` into a non-secret field — the
server-side resolver only treats `••••••` as a sentinel for fields whose schema marks
them secret or connection-typed, so other fields pass through untouched.

`executeTask(workspace, taskId, input, { sourceJobId })` adds `source_job_id` to the
request body when set.

### Lineage UI (`ui/src/pages/job-detail.tsx`)

Add to the existing `InfoGrid`, mirroring the retry-of/retried pattern:

- When `source_type === 'rerun'`: a **Re-run of** entry linking to `source_job_id`.
- When `source_type === 'restart'` (future): a **Restart of** entry, plus the resumed
  step name. Out of scope for feature A code, but the field is reserved.

The inverse direction (showing all re-runs *of* a given job) requires a list query and
is out of scope for feature A.

### Legacy-job fallback

When `source.raw_input === null` on the source job, the Re-run Link is still rendered
and still navigates. The task page detects the missing prefill and shows a small info
banner:

> This job predates Re-run prefill — defaults shown.

Form behaves like a normal new run; `source_job_id` is **not** included in the submit
(server would 400 anyway).

## Section 5 — Tests, Edge Cases, Rollout

### Server unit tests

- `crates/stroem-common/src/template.rs::tests::test_resolve_rerun_sentinels_*`:
  - secret field with sentinel + source value present → replay
  - secret field with sentinel + no source value → remove (default kicks in)
  - connection field with sentinel + source value → replay
  - non-sentinel values → untouched
  - non-secret/non-connection sentinel → untouched (sentinel only meaningful for those types)

- `crates/stroem-server/src/web/api/jobs.rs::tests::test_redact_response_includes_raw_input`:
  - `raw_input` secret values redacted
  - `raw_input` connection names preserved
  - `raw_input = null` round-trips correctly

### Server integration tests

`crates/stroem-server/tests/integration_test.rs`:

- End-to-end Re-run with a custom secret + custom connection: assert the new job's
  resolved `input` matches the source's, and the new job's `raw_input` records the
  same shape (with secret redacted on response).
- Re-run rejection cases: unknown `source_job_id` → 400; cross-workspace
  `source_job_id` → 400; source with `raw_input = NULL` → 400.
- ACL: re-run as user-without-View on source job → 403.

### Frontend tests

No unit tests today; covered by Playwright e2e.

`ui/e2e/jobs.spec.ts`:

- **Plain field prefill**: trigger `data-pipeline` with a non-default `data` value,
  click Re-run, assert `#input-data` is populated with that value.
- **Lineage badge**: after Re-run, the new job's detail page shows a "Re-run of" link
  pointing back to the source.
- **Connection-type prefill** and **secret prefill**: require fixture tasks with those
  field types. If existing fixtures don't cover them, add a minimal fixture in
  `workspace/.workflows/` (e.g., a `rerun-fixture.yaml` task with one connection-typed
  input and one secret input). Document this in the implementation plan.

### Edge cases captured

- **`for_each` / templated step inputs**: unchanged. This change touches only task
  input, not flow-step input.
- **Connection deleted between original run and Re-run**: server's existing
  `resolve_connection_inputs` raises a clear error → surfaces as 400 on the rerun
  execute. UI shows the error inline via the existing `submitError` path.
- **Workspace revision drift**: the new job uses the *current* workspace revision, not
  the source's. Re-run means "with current code" — documented in the user-facing guide.
- **Concurrency**: no special handling. Two simultaneous re-runs of the same source
  both succeed and create independent new jobs.
- **Source job with `source_type = 'rerun'`**: re-running a re-run is allowed; no chain
  truncation. `source_job_id` always points to the immediate predecessor.
- **Plain-string field containing a `ref+` reference**: the existing `redact_json`
  redacts `ref+...` patterns to `••••••` even outside secret-typed fields. On Re-run,
  the UI receives `••••••` for that plain field, prefills it as such, and submits
  `••••••` unchanged. The server's `resolve_rerun_sentinels` only treats `••••••` as a
  sentinel for secret/connection-typed fields, so the value is stored verbatim — the
  user loses the original `ref+` reference unless they re-type it. Workaround: use a
  secret-typed field for `ref+` values. Documented as a known limitation in the
  user-facing guide; not addressed in this feature.

### Rollout

Single PR. Migration `032_job_raw_input_and_lineage.sql`. No feature flag — the change
is invisible to users on jobs created before the migration (they render as "predates
prefill") and active for new jobs.

## Documentation Updates

Per `CLAUDE.md` mandatory-doc rule:

- `CLAUDE.md`: add a short note under "Database" or a new "Job Lineage" subsection
  describing `raw_input`, `source_job_id`, the extended `source_type` enum, and the
  retry-vs-rerun-vs-restart distinction.
- `docs/src/content/docs/guides/`: no existing UI guide to update; the Re-run prefill
  is a UX detail discoverable from the button. If we later add a "Running tasks from
  the UI" guide, prefill goes there.
- `docs/internal/stroem-v2-plan.md`: append the feature-A status when implementation
  lands.

## Open Questions

None at design time. Implementation plan will pick up:

- Exact SQL migration file content.
- Whether `resolve_rerun_sentinels` lives in `template.rs` or `job_creator.rs` (lean
  `template.rs` since it's pure, schema-aware, and testable in isolation).
- Whether to lazy-render the lineage banner via a list query or a denormalized
  `has_reruns` flag (out of scope for now — link is one-directional).
