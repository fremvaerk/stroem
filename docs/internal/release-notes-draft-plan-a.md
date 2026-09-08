# Release notes draft — Plan A (creation-settlement unification)

Patch release, independently releasable. No config or schema changes required.

## User-visible changes

- **Workspace-level hooks now fire for Re-run jobs.** Previously `on_success` / `on_error` /
  `on_cancel` hooks defined at the workspace level only fired for `api`, `user`, `trigger`,
  `webhook`, `mcp`, and `retry` jobs. A job created via the UI's **Re-run** button (`source_type
  = "rerun"`) now counts as top-level too, so workspace hooks fire for it the same way they do
  for the original run. `restart` (reserved, not yet user-facing) is included for the same reason.
  The workspace-level `on_suspended` fallback follows the same classifier, so an approval gate in
  a Re-run job now fires the workspace `on_suspended` hook too.
- **A job whose only non-completed step was cancelled now settles as `cancelled`, not
  `completed`.** If every step in a job reaches a terminal state and at least one step is
  `cancelled` (and no untolerated step failed), the job now settles `cancelled`. Previously such
  a job could settle `completed`, hiding the cancellation from the job list and from `on_cancel`
  hooks.
- **`continue_on_failure` does not tolerate a `cancelled` step.** The new `cancelled` rule runs
  after the untolerated-failure check and is deliberately not gated by `continue_on_failure`: a
  flow step marked `continue_on_failure: true` whose `type: task` child job is cancelled now
  cancels the whole job, where it previously completed. `continue_on_failure` tolerates failures,
  not cancellations — a cancellation is an operator decision to stop, not a step outcome to work
  around.
- **Jobs that settle immediately at creation now fire hooks, archive logs, and count in
  metrics.** A job that has no steps to run (or whose steps are already terminal at creation
  time) used to skip terminal handling entirely — no `on_success`/`on_error`/`on_cancel` hook,
  no log archive upload, no `stroem_jobs_completed_total` increment. It now goes through the same
  terminal-handling path as every other job.
- **Job initialisation errors are recorded on the job's steps instead of returning a bare 500.**
  If a job commits successfully but a post-commit initialisation step then fails, the job is
  marked `failed` with a step-level error message (`[creation] initialisation failed: …`) instead
  of leaving a `pending` job behind with no indication of what went wrong. The job row and every
  non-terminal step are written in one transaction. A DB outage during that compensation still
  surfaces as a 500. Approval-step dispatch and final settlement are covered by the same
  wrapper, so a transient failure can no longer leave an approval step `ready` forever.
- **A job that settles at creation now honours `continue_on_failure` and aggregates step
  output.** Creation-time settlement uses the same rules as the orchestrator. Previously it
  failed the job on any failed step (ignoring `continue_on_failure`) and completed with no
  output.
- **Hook chains are bounded at depth 3.** The `source_type = "hook"` recursion guard does not
  stop an indirect cycle: a hook whose action is `type: task` creates a hook job whose own
  `type: task` step creates an ordinary child, and that child's task-level hooks fire again. A
  job with three or more `hook` links in its ancestry no longer fires hooks; the suppression is
  written to the job's log view as `[hooks] hook chain depth limit (3) reached`.

## Internal / operational

- Terminal side effects (hook dispatch, task-level retry job creation, parent-job propagation,
  Prometheus counter, log archive upload) now run behind a single CAS claim
  (`claim_terminal_handling`), rather than the Prometheus counter being the only CAS-guarded
  effect. The guarantee is **at-most-one claimant across replicas**, not exactly-once delivery:
  side effects that fail after the claim is taken are logged to the job's server log and are not
  retried.
- A `type: task` job that settles synchronously during its own creation (rather than via normal
  step completion) is now reconciled against its parent step immediately, instead of potentially
  leaving the parent stuck `running`. The reconciliation walks the whole descendant chain (up to
  the 10-level task nesting cap), so a grandchild that settles at creation also unblocks the
  intermediate job, not just a direct child.
- An agent `type: agent` task-tool call whose child job would be terminal the moment it is
  created is now rejected with an error instead of returning a child id the agent step would
  wait on forever. The child's own subtree is reconciled first, so a child that settles because
  a nested grandchild settled is caught by the same check.
- Propagation of an `agent_tool` child into its parent agent step is gated on a registration
  barrier: until the worker has recorded the child id in the step's `agent_state`, propagation
  is deferred rather than falling through to ordinary step completion (which could settle an
  agent step whose worker is still running). `agent-state` and `suspend` writes replay the
  deferred propagation for any pending child that is already terminal.
- Terminal side effects wait for execution to drain. A job row can be terminal while its workers
  still run (a cancel stamps `cancelled` immediately), so all three convergence sites now skip the
  terminal block — and the claim — while any step is `running` or `claimed`. The last worker
  acknowledgement drains the job and takes the claim. Without this the first worker's completion
  closed and archived the log while a sibling was still emitting, and the later completion, having
  lost the one-shot claim, could no longer refresh it.
- Reconciliation of settled descendants now requires execution quiescence: a terminal descendant
  with a `running` or `claimed` step of its own is skipped, so a cancelled child keeps its
  cancellation signal and goes on collecting log lines until its worker acknowledges.

## Not included

- Task-level retry is untouched by this release and remains broadly non-functional: `max_retries`
  is never persisted at job creation, so a task-level `retry:` config never fires for a
  production-created job, and retry does not replay `raw_input` (rotated/deleted connections
  still supply stale resolved values to retry jobs). Tracked in `docs/internal/TODO.md`.
