# Release notes draft — Plan A (creation-settlement unification)

Patch release, independently releasable. No config or schema changes required.

## User-visible changes

- **Workspace-level hooks now fire for Re-run jobs.** Previously `on_success` / `on_error` /
  `on_cancel` hooks defined at the workspace level only fired for `api`, `user`, `trigger`,
  `webhook`, `mcp`, and `retry` jobs. A job created via the UI's **Re-run** button (`source_type
  = "rerun"`) now counts as top-level too, so workspace hooks fire for it the same way they do
  for the original run. `restart` (reserved, not yet user-facing) is included for the same reason.
- **A job whose only non-completed step was cancelled now settles as `cancelled`, not
  `completed`.** If every step in a job reaches a terminal state and at least one step is
  `cancelled` (and no untolerated step failed), the job now settles `cancelled`. Previously such
  a job could settle `completed`, hiding the cancellation from the job list and from `on_cancel`
  hooks.
- **Jobs that settle immediately at creation now fire hooks, archive logs, and count in
  metrics.** A job that has no steps to run (or whose steps are already terminal at creation
  time) used to skip terminal handling entirely — no `on_success`/`on_error`/`on_cancel` hook,
  no log archive upload, no `stroem_jobs_completed_total` increment. It now goes through the same
  terminal-handling path as every other job.
- **Job initialisation errors no longer return a 500 over a job that was already created.** If a
  job commits successfully but a post-commit initialisation step then fails, the job is now
  marked `failed` with a step-level error message (`[creation] initialisation failed: …`) instead
  of returning an HTTP 500 while leaving a `pending` job behind with no indication of what went
  wrong.

## Internal / operational

- Terminal side effects (hook dispatch, task-level retry job creation, parent-job propagation,
  Prometheus counter, log archive upload) are now exactly-once across replicas via a single CAS
  claim (`claim_terminal_handling`), rather than the Prometheus counter being the only
  CAS-guarded effect.
- A child `type: task` job that settles synchronously during its own creation (rather than via
  normal step completion) is now reconciled against its parent step immediately, instead of
  potentially leaving the parent stuck `running`.

## Not included

- Task-level retry does not yet replay `raw_input` (rotated/deleted connections still supply
  stale resolved values to retry jobs) — tracked separately, not part of this release.
