---
title: Upgrading to 0.17 — dependency flags
description: continue_on_failure and continue_when_skipped are now read only from the dependency; strict-AND merges replace automatic convergence
---

Release 0.17.0 changes how `continue_on_failure` and `continue_when_skipped`
decide whether a step runs. This is a breaking behaviour change — no schema
migration is involved, but existing workflows can now skip or run steps
differently than they did on 0.16.x. Read this page before upgrading if any
of your workflows use either flag, or rely on an if/else merge running
automatically.

## The rule

> **`continue_on_failure`** on a step: if this step fails, is cancelled, or is skipped because something above it failed, the steps that depend on it still run, and the failure does not fail the job. It never makes the step itself run.
> **`continue_when_skipped`** on a step: if this step is skipped by its own `when`, an empty `for_each`, or because a step above it was skipped the same way, the steps that depend on it still run.
> A step runs only when **every** dependency lets it through (completed, or not completed but carrying the matching flag). A failure fails the job unless a `continue_on_failure` catches it — on the failing step or on every path below it.

Both flags are read **only from the dependency**, never from the dependent — a step's own flags never make *it* run. There is no automatic convergence: a dependency skipped by choice blocks its dependents even when a sibling dependency completed. An if/else merge needs `continue_when_skipped` on **each** branch step.

## Behaviour changes

| Workflow shape | 0.16.x | 0.17 | Fix |
|---|---|---|---|
| `a` fails (cof), `b` depends on `a` (no flag) | `b` skipped, job green | `b` runs, job green | — (was the bug) |
| `a` fails (no flag), `b` depends on `a` (cof) | `b` runs, job failed | `b` skipped `unreachable`, `b`'s dependents run, job **completed** (caught at `b`) | cof on `a` if `b` must run; cleanup that must not hide the failure → `on_error` |
| `a` fails (no flag), nothing depends on `a` | job failed | job failed | — |
| if/else merge, branches without cws | merge runs | merge skipped `cascade` | cws on each branch step |
| merge after `x` completed + `y` skipped by choice (no cws) | merge runs | merge skipped `cascade` | cws on `y` |
| `stroem run`, untolerated failure | run aborts | independent branches continue | — |

A merge that is now skipped also loses its entry in the job output (job
output = outputs of the flow's terminal steps).

A job whose failure is now caught downstream (row 2) ends `completed`
instead of `failed`, and every terminal side effect follows the status:
`on_success` fires instead of `on_error`, no task-level retry job is created
even when the task has `retry`, and `stroem_jobs_completed_total` counts it
under `status="completed"`.

Unchanged on purpose: a cancelled step (e.g. a cancelled child job under a
`type: task` step) still makes the job `cancelled` — `on_cancel`, no retry —
even though its dependents are skipped `unreachable`. Skipped rows never
decide job status, whatever their reason; only `failed` and `cancelled` rows
do.

## Worked example: prod job `9691df79`

`jobs/recalc-pipeline`, 2026-09-25. Flow shape:
`ml-prediction-beta` (no flags) → `ml-impressions-beta` (`continue_on_failure`)
→ `merge-ml` (depends on impressions master, beta, stage) → `agg-sessions` → …

| Step | Before (0.16.x) | After |
|---|---|---|
| `ml-prediction-beta` | failed | failed |
| `ml-impressions-beta` | **ran** (its own flag, dependent-side) | skipped `unreachable` — its own flag does not make it run |
| `merge-ml` | ran | runs — `ml-impressions-beta`'s skip is failure-class and it carries `continue_on_failure` → Pass |
| job | failed | **completed** — `ml-prediction-beta`'s only dependent, `ml-impressions-beta`, catches the failure; the failure is listed as tolerated |

`continue_on_failure` on `ml-prediction-beta` itself would instead let
`ml-impressions-beta` run on the failed prediction — not what the workflow's
author wants: the comment on that step says "a failed prediction still skips
impressions", and 0.17 is the first release where the server actually
behaves that way.

## Hook payload

Three related changes to the `hook.*` template context (see [Hooks](/guides/hooks/)):

- `hook.failed_steps[]` gains a new field, `tolerated: bool` — `true` when that failure is caught by `continue_on_failure`, on the failing step itself or on every path below it.
- A loop instance row's `continue_on_failure` in `hook.failed_steps[]` now reports its **placeholder's** flag, not a hardcoded `false` as before — a failed `p[0]` under a placeholder `p` that has `continue_on_failure` now shows `continue_on_failure: true`.
- `on_success` hooks can now see a non-empty `hook.failed_steps` — a job that completed with a caught failure fires `on_success`, and that failure is still listed (with `tolerated: true`) so the hook can report it.

## Checklist

- [ ] Move `continue_on_failure` from cleanup/notify steps to `on_error` (or `on_cancel`) hooks. There is no dependent-side "run even if upstream failed" flow step — see [Hooks](/guides/hooks/#cleanup-and-notification-after-a-failure).
- [ ] Add `continue_when_skipped` to **each** branch step of every if/else merge — a completed sibling no longer covers for an unflagged skipped one.
- [ ] Run `stroem validate` — it now warns about exactly this shape: `Task 't' step 'm' will be skipped whenever 'd' is skipped (add continue_when_skipped: true to 'd' to let 'm' run)`.
- [ ] Search your workflows for `continue_on_failure` on a step that has a `depends_on` and check whether the flag was meant to protect *that step's own* dependents (0.17 behaviour) or make the step itself tolerate a failed dependency (needs the flag on the dependency instead).
- [ ] Existing jobs in flight at upgrade time are re-evaluated under the new rule on their next cascade — a job mid-flight can see a merge skipped that 0.16.x would have run.

See also: [Conditionals](/guides/conditionals/) for the full flag reference and patterns, and [Migration 046](/operations/migration-046/) for the earlier (0.16.2/0.16.3) move of `continue_when_skipped` onto the dependency.
