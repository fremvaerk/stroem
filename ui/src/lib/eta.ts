import type { JobDetail, JobStep, TaskStatsResponse } from "./types";

/**
 * Result of an ETA computation.
 *
 *  - `eta`     : the job is expected to finish in `remainingMs` more.
 *  - `overrun` : the job has already been running longer than the p50 reference,
 *                by `overrunMs`. UI typically shows this as a warning badge
 *                rather than a countdown.
 *  - `null`    : insufficient data to make a meaningful prediction
 *                (no historical sample, or job isn't running).
 */
export type EtaResult =
  | { type: "eta"; remainingMs: number }
  | { type: "overrun"; overrunMs: number; referenceMs: number }
  | null;

export interface ComputeEtaArgs {
  job: JobDetail;
  stats: TaskStatsResponse | null;
  /** Epoch milliseconds. Injectable for tests. */
  now: number;
  /** Minimum sample size before we'll predict. Default 5. */
  minSample?: number;
}

const DEFAULT_MIN_SAMPLE = 5;

/**
 * Map step_name → step stats lookup for O(1) reads inside the algorithm.
 *
 * NOTE: for_each instance step rows have names like `mystep[0]`, `mystep[1]`.
 * The backend aggregates only top-level rows (loop_source IS NULL), so the
 * placeholder `mystep` will appear in stats with the full loop duration —
 * that's what we want for ETA. Instances themselves won't have stats and will
 * be ignored by `getStepP50`.
 */
export function indexSteps(stats: TaskStatsResponse): Map<string, number> {
  const idx = new Map<string, number>();
  for (const s of stats.steps) {
    if (s.p50_ms != null) idx.set(s.step_name, s.p50_ms);
  }
  return idx;
}

export function getStepP50(
  idx: Map<string, number>,
  step: JobStep,
): number | null {
  return idx.get(step.step_name) ?? null;
}

/**
 * Compute an ETA for a running job based on historical duration percentiles.
 *
 * Inputs available to you:
 *  - `job.status`              : "pending" | "running" | "completed" | ...
 *  - `job.started_at`          : ISO string or null
 *  - `job.steps`               : array of JobStep with .status, .started_at, .completed_at
 *  - `stats.task.p50_ms`       : median full-task duration over recent runs
 *  - `stats.task.sample_size`  : how many runs the percentiles are based on
 *  - `stats.steps[i].p50_ms`   : per-step median (use `getStepP50(stepIdx, step)`)
 *
 * Design choices YOU need to make:
 *
 *  1) **Algorithm**.
 *     (a) FLAT:        eta = task.p50_ms - (now - job.started_at)
 *     (b) STEP-WEIGHTED:
 *           let running = the currently-running JobStep (if any)
 *           let remaining = sum of p50 for all pending/ready steps
 *           let runningRemaining = max(0, runningStepP50 - elapsedOnRunning)
 *           eta = runningRemaining + remaining
 *     (b) is more accurate but is meaningless when steps don't have stats yet.
 *     Consider a hybrid: use STEP-WEIGHTED when every running+pending step has
 *     stats, else fall back to FLAT.
 *
 *  2) **Overrun handling**.
 *     When elapsed > reference, do you return:
 *       - { type: "overrun", overrunMs, referenceMs }   (recommended — honest)
 *       - { type: "eta", remainingMs: 0 }               (always optimistic)
 *       - { type: "eta", remainingMs: -overrunMs }      (caller renders)
 *
 *  3) **Minimum confidence**.
 *     Return null if sample_size < minSample. Already enforced for you below.
 *
 *  4) **Pending jobs (not yet started)**.
 *     If job.status === "pending", elapsed is 0 — you can either return null
 *     (no ETA until it starts) or return task.p50_ms as a best-effort.
 *
 * Keep the function pure and side-effect-free. Tests in `eta.test.ts` cover
 * the cases above; if you change the contract, update the tests.
 */
export function computeEta(args: ComputeEtaArgs): EtaResult {
  const { job, stats, now, minSample = DEFAULT_MIN_SAMPLE } = args;

  // --- Preconditions ---
  if (!stats || stats.task.sample_size < minSample) return null;
  if (job.status !== "running" && job.status !== "pending") return null;
  const taskP50 = stats.task.p50_ms;
  if (taskP50 == null) return null;

  const stepIdx = indexSteps(stats);

  const elapsedTotalMs = job.started_at
    ? Math.max(0, now - new Date(job.started_at).getTime())
    : 0;

  // ── Overrun has priority over ETA ───────────────────────────────────────
  // Compute against the task-level reference so the message stays meaningful
  // even when individual step stats are missing.
  if (elapsedTotalMs > taskP50) {
    return {
      type: "overrun",
      overrunMs: elapsedTotalMs - taskP50,
      referenceMs: taskP50,
    };
  }

  // ── Step-weighted estimate ──────────────────────────────────────────────
  // Use it when the currently-running step has stats AND every not-yet-started
  // step also has stats. If any step lacks stats, the sum would be a silent
  // underestimate, so we fall back to a flat task p50 instead.
  const runningStep = job.steps.find((s) => s.status === "running");
  const pendingSteps = job.steps.filter(
    (s) => s.status === "pending" || s.status === "ready",
  );

  if (runningStep) {
    const runningP50 = getStepP50(stepIdx, runningStep);
    const allPendingHaveStats = pendingSteps.every((s) =>
      stepIdx.has(s.step_name),
    );

    if (runningP50 != null && allPendingHaveStats) {
      const elapsedOnRunning = runningStep.started_at
        ? Math.max(0, now - new Date(runningStep.started_at).getTime())
        : 0;
      const runningRemaining = Math.max(0, runningP50 - elapsedOnRunning);
      const pendingTotal = pendingSteps.reduce(
        (sum, s) => sum + (stepIdx.get(s.step_name) ?? 0),
        0,
      );
      return { type: "eta", remainingMs: runningRemaining + pendingTotal };
    }
  }

  // ── Flat fallback ──────────────────────────────────────────────────────
  // Either no step is running yet (pending job) or step stats are incomplete.
  return { type: "eta", remainingMs: Math.max(0, taskP50 - elapsedTotalMs) };
}
