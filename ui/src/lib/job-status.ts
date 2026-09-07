/** Job statuses that admit no further step transitions. */
const TERMINAL_JOB_STATUSES = ["completed", "failed", "cancelled"];

/** True when the job has settled and may be restarted from a step. */
export function isTerminalJobStatus(status: string | undefined | null): boolean {
  return status != null && TERMINAL_JOB_STATUSES.includes(status);
}
