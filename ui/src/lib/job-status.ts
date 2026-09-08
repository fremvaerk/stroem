/** Job statuses that admit no further step transitions. */
const TERMINAL_JOB_STATUSES = ["completed", "failed", "cancelled"];

/** True when the job has settled and may be restarted from a step. */
export function isTerminalJobStatus(status: string | undefined | null): boolean {
  return status != null && TERMINAL_JOB_STATUSES.includes(status);
}

/**
 * Source types whose jobs are *derived* runs rather than a user's own top-level
 * run. Mirrors `DERIVED_SOURCE_TYPES` in `crates/stroem-server/src/web/api/jobs.rs`.
 */
const DERIVED_SOURCE_TYPES = ["hook", "task", "agent_tool", "upload"];

/**
 * True when the job is a user's own top-level run, and so may be re-run or
 * restarted. Both actions always create a parentless job, so offering them on a
 * `type: task` child or an agent tool call would detach the new job from the
 * parent that is waiting on it, and offering them on a hook job would relabel it
 * `rerun`/`restart` — source types the server treats as top-level, re-enabling
 * the workspace-hook fanout that `hook` exists to suppress. The server rejects
 * both with 400; this hides the controls so the rejection is never reached.
 */
export function isTopLevelJob(job: {
  parent_job_id?: string | null;
  source_type?: string | null;
}): boolean {
  if (job.parent_job_id != null) return false;
  return job.source_type == null || !DERIVED_SOURCE_TYPES.includes(job.source_type);
}
