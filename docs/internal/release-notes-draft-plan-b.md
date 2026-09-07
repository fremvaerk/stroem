# Release notes draft — Plan B (Restart From Step)

Ships on top of Plan A (creation-settlement unification). Additive migration
(`045_job_step_carried_over.sql`), new endpoint, new UI action — no config changes
required.

**Ships together with Plan A.** These notes cover only what Plan B adds. The Re-run
workspace-hook change, the `cancelled` settlement change and the two stuck-job fixes
(jobs that settle at creation, and a `type: task` child that settles at creation without
propagating to its parent) are documented in `release-notes-draft-plan-a.md`; merge both
drafts into the combined release notes so none of them is lost.

## User-visible changes

- **New: Restart from a step.** A terminal job's step detail panel (and a `for_each`
  loop's group header) now has a **Restart from here** button. It creates a new job that
  reruns only the chosen step and everything downstream of it, while every other step is
  carried forward from the source job exactly as it ended — no need to re-run an entire
  pipeline to retry one late failure. Uses the same input as the source job (no form) and
  the current workspace revision. A confirmation dialog previews what will rerun vs. be
  carried over, and warns when a carried-over failure won't be tolerated by the current
  flow. See the new [Re-running and Restarting Jobs](../src/content/docs/guides/rerun-and-restart.md)
  guide and `POST /api/jobs/{id}/restart` in the API reference.
  - Known limitations (documented in the guide): state snapshots resolve to the latest
    snapshot at claim time, not what the source job saw; artifacts are not carried over;
    a carried cross-workspace step keeps the current action definition but the old
    output; child jobs of a carried `type: task` step are not re-linked to the new job.
- **Re-run and Restart are now offered only on top-level jobs.** Both actions always create
  a brand-new job with no parent, so offering them on a `type: task` child job, an agent
  tool call, an uploaded-state job or a hook job produced a detached job tree that nothing
  propagated back to the original parent — and restarting a hook job relabelled it
  `restart`, a source type the server treats as top-level, re-enabling the workspace-hook
  fanout the `hook` source type exists to suppress. The buttons are now hidden on those
  jobs and both APIs reject them with `400`. This closes a pre-existing hole in Re-run as
  well as the new one Restart would have added.
- **Restart rejects input that no longer satisfies the task schema.** Restart replays the
  source job's `raw_input` with no form in front of it. If the task has since gained a
  required input field with no default, the restart is now rejected with `400` naming the
  missing field, instead of creating a job with a hole in its input. Use Re-run, which
  gives you the form, to supply the new value.
- **`hook.failed_steps[].carried_over`.** In an `on_error` hook, each failed-step entry
  now has a `carried_over` boolean so a notification can tell a fresh failure apart from
  one carried forward, unactioned, by a restart. `false` for every job that isn't a
  restart.
- **Restart jobs are excluded from duration statistics and ETA.** Whole-job and per-step
  duration stats (used for percentile charts and the running-job ETA pill) no longer
  count restart jobs, even a restart from the first step. A restart job's ETA never falls
  back to the whole-task p50 the way a normal job's does — it shows a step-weighted
  estimate when full stats are available for its rerun steps, or nothing.
- **Hook chain depth budget fixed for long `type: task` chains.** The guard that stops
  runaway hook cycles (`on_error` → `type: task` hook → child's own `on_error` → ...) was
  previously budgeted in raw ancestry hops (20), which a sufficiently deep but legitimate
  chain of plain `type: task` levels between hook links could exhaust before the real
  3-hook-link limit was ever reached — silently leaving that particular cycle shape
  unbounded. The budget is now sized in hook links (`3 hook links × 11 task-nesting
  levels + 1`), so the limit fires only when actual hook cycling happens, not because of
  unrelated task nesting depth.

## Internal / operational

- Terminal-handling exactly-once guarantee under cancellation is now covered by a test
  that can actually fail without it: cancelling a parent whose child step is unclaimed
  races two independent `cancel_job` calls for the same `on_cancel` hook dispatch, and
  the fix (an atomic claim) is verified to prevent a duplicate hook firing.
- `job_step.carried_over` (migration `045_job_step_carried_over.sql`) marks a row copied
  forward by a restart rather than executed by the job it belongs to; surfaced in
  `GET /api/jobs/{id}` per step.

## Not included

- `carried_failed_tolerated` — the list of carried-over failures the current flow *does*
  tolerate — is returned by the dry-run restart response but not yet shown in the confirm
  dialog UI.
- Task-level retry's `raw_input`-replay hygiene (rotated/deleted connections still supply
  stale resolved values to retry jobs) remains open, tracked alongside the restart
  follow-ups in `docs/internal/TODO.md`.
