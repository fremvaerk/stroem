---
title: Re-running and Restarting Jobs
description: Re-run a job from scratch with the same input, or restart it from a specific step and skip work that already succeeded
---

Two ways to recover from a failed (or otherwise finished) job without retyping input: **Re-run**, which starts a brand-new job from the beginning, and **Restart from a step**, which reruns only the steps affected by a chosen step and carries the rest forward unchanged.

Both are available from the job detail page once a job reaches a terminal state (`completed`, `failed`, or `cancelled`), and only on **top-level** jobs — a job you started yourself, or one a trigger, webhook, schedule or retry started. A child job of a `type: task` step, an agent tool call, or a hook job cannot be re-run or restarted: both actions create a brand-new job with no parent, so the original parent would never learn about it. Restart or re-run the top-level job instead. The buttons are hidden on those jobs, and the API rejects them with `400`.

## Re-run

The **Re-run** button on a job's detail page opens the task's execute form prefilled with that job's original input (`raw_input`), including connection selections and non-default secret fields. Adjust anything you like, then submit — it creates a normal new job via `POST /api/workspaces/{ws}/tasks/{name}/execute`, running every step from scratch on the current workspace revision.

The new job records `source_type: "rerun"` and `source_job_id` pointing at the job it was re-run from, so job detail shows a **Re-run of** link back to the source. Re-run is only offered for jobs whose `raw_input` was captured — jobs created before this feature (or via legacy clients) show "This job predates Re-run prefill" and fall back to blank defaults.

Use Re-run when you want to change input, or when the failure is unrelated to which steps already succeeded. To skip re-running steps that already succeeded, use Restart from a step instead.

## Restart from a step

**Restart from a step** creates a new job that reruns only a chosen step and everything downstream of it, while every other step is copied forward from the source job exactly as it ended. It uses the same input as the source job (no form) and the current workspace revision. If the task's input schema has gained a **required** field with no default since the source job ran, the replayed input no longer satisfies it and the restart is rejected with `400` — use Re-run instead, which gives you the form.

### Where to find it

- On a normal step's detail panel, a **Restart from here** button appears once the job is terminal (next to the approval card, if any).
- For a `for_each` loop, the button is on the loop's group header instead — individual loop instances are always restarted through the placeholder, never on their own.
- The button only appears if you have Run permission on the task (the same permission Execute requires).

Clicking it opens a confirmation dialog built from a dry-run preview (`POST /api/jobs/{id}/restart` with `dry_run: true`), so the counts and step names always reflect the *current* flow, not the flow the source job ran:

> Restart **{task}** from **{step}**?
> Reruns *N* step(s): *step-a, step-b, ...*. *M* step(s) are carried over unchanged.

The preview is computed when you open the dialog and the create request recomputes it, so a workspace reload in between can change what actually runs. The `restart_steps` in the create response is the authoritative list.

If any carried-over step ended failed and the current flow does not tolerate it (no `continue_on_failure`), the dialog adds a warning that the new job will still end failed, and suggests restarting from an earlier step to rerun those failures too.

### What gets rerun vs. carried over

Given the step you chose:

- The chosen step, plus any step that has been **added to the flow** since the source job ran (it has no matching row in the source job), form the initial set of roots.
- Every step that transitively depends on any of those roots is also rerun.
- Everything else is **carried over**: its new row is stamped with the source job's ending status, output, and error message verbatim, without actually running again. A carried step that was still in progress when the source job was cancelled comes over as `cancelled`.
- Steps that existed in the source job but have since been removed from the flow are dropped, same as Re-run.
- Automatic inclusion is keyed on step **names**: a step counts as "added" only if its name has no matching row in the source job. A step that was removed and later re-added under the same name still matches, so it is carried over, not rerun — restart from it explicitly if its definition changed. When a genuinely new name sits upstream of a step you were keeping, that downstream step is pulled back into the rerun set, so you can't strand a carried step behind a step that has no matching prior run.

Carried-over steps show a muted **carried over** badge in the step timeline. Their detail panel doesn't fetch logs for the new job — it links back to the source job's logs instead, since nothing executed here.

### Job settlement

The new job runs through the normal creation, promotion, and dispatch pipeline for its rerun steps. If the rerun set collapses immediately (for example every rerun step is unreachable because a carried dependency is `failed` and not tolerated), the job settles right away, and hooks and metrics still fire exactly once for that outcome — same as any other job that finishes at creation time.

A carried `failed` step whose current flow step is `continue_on_failure: true` does not fail the job. A carried `cancelled` step with no failures ends the job `cancelled`, not `completed`.

## Limitations

Restart from a step does not fully reconstruct the source job's environment for carried-over steps:

- **State snapshots** (`{{ state.* }}`) resolve to the *latest* snapshot for the task at claim time, not whatever snapshot existed when the source job ran.
- **Artifacts** are not copied. A rerun step that reads `/artifacts/` from a step that was carried over (rather than actually rerun) finds nothing there, because artifacts belong to the job that produced them.
- **Revision drift**: carried-over output was produced under the source job's workspace revision, but the rerun steps run under the current one. If templates or scripts changed incompatibly, a rerun step consuming a carried step's output can fail at render or run time.
- **Cross-workspace carried steps** keep the *current* action definition (in case it changed) but the *old* output — the only link back to what actually produced that output is the job's `source_job_id`.
- **Secret redaction** on carried output and error messages only covers currently configured connection/secret values, same limitation as job input generally — a value from a since-removed or rotated connection, or a user-typed secret, is not redacted.
- **Child jobs** created by a carried `type: task` step are not re-linked to the new job; only the step's final output comes along.
- **Duration statistics**: restart jobs (whole job and per-step) are excluded from the task duration stats used for percentile charts and ETA, even when the restart reran from the very first step. A restarted job also never falls back to the whole-task p50 for its ETA — it either computes a step-weighted estimate from steps it's actually rerunning, or shows none.

If a failure you want to retry isn't in the set you'd get by restarting from where it happened (because an earlier carried step is also failed and untolerated), restart from that earlier step instead so it reruns too.

## Related

- [Hooks](/stroem/guides/hooks) — workspace-level hooks fire for both `rerun` and `restart` jobs (they're top-level sources), and `hook.failed_steps[].carried_over` flags failures that came from the source job rather than this run.
- [Retry Mechanisms](/stroem/guides/retry) — automatic step/task retry, a different mechanism for handling failures without user intervention.
- [API Reference](/stroem/reference/api) — `POST /api/jobs/{id}/restart` request/response shapes.
