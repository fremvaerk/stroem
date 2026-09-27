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

/**
 * The job this one was created from: the source of a Re-run or Restart, the
 * FIRST attempt of a task Retry (`retry_of_job_id` is the chain root, not the
 * previous attempt), the parent of a `type: task` / agent-tool child, or the
 * job whose terminal state (or suspended step) fired a hook.
 */
export interface JobLineage {
  kind: "rerun" | "restart" | "retry" | "child" | "hook";
  jobId: string;
  /** Restart: the step it began at. Child: the parent step that started it. */
  step: string | null;
}

/**
 * Where this job came from, when it was created from another job. A job with a
 * parent is a child whatever its source type (the same rule as
 * `isTopLevelJob`); otherwise it is keyed on `source_type`, so a lineage column
 * the row happens to carry for another reason never mislabels the job. A
 * source type whose pointer is missing yields `null` rather than a broken link.
 */
export function jobLineage(job: {
  source_type: string;
  source_job_id: string | null;
  restart_from_step: string | null;
  retry_of_job_id: string | null;
  parent_job_id: string | null;
  parent_step_name: string | null;
}): JobLineage | null {
  if (job.parent_job_id) {
    return { kind: "child", jobId: job.parent_job_id, step: job.parent_step_name };
  }
  switch (job.source_type) {
    case "rerun":
      if (!job.source_job_id) return null;
      return { kind: "rerun", jobId: job.source_job_id, step: null };
    case "restart":
      if (!job.source_job_id) return null;
      return { kind: "restart", jobId: job.source_job_id, step: job.restart_from_step };
    case "retry":
      if (!job.retry_of_job_id) return null;
      return { kind: "retry", jobId: job.retry_of_job_id, step: null };
    case "hook":
      if (!job.source_job_id) return null;
      return { kind: "hook", jobId: job.source_job_id, step: null };
    default:
      return null;
  }
}
