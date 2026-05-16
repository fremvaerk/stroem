import { describe, it, expect } from "vitest";
import { computeEta, indexSteps } from "../eta";
import type { JobDetail, JobStep, TaskStatsResponse } from "../types";

// Pin a stable "now" so tests are deterministic.
const NOW = new Date("2026-01-01T12:00:00.000Z").getTime();

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

function mkStep(partial: Partial<JobStep> & { step_name: string }): JobStep {
  return {
    step_name: partial.step_name,
    action_name: partial.action_name ?? "act",
    action_type: partial.action_type ?? "script",
    action_image: null,
    runner: "local",
    input: null,
    output: null,
    status: partial.status ?? "pending",
    worker_id: null,
    started_at: partial.started_at ?? null,
    completed_at: partial.completed_at ?? null,
    suspended_at: null,
    error_message: null,
    when_condition: null,
    depends_on: [],
    for_each_expr: null,
    loop_source: null,
    loop_index: null,
    loop_total: null,
    retry_attempt: 0,
    max_retries: null,
    retry_history: [],
    retry_at: null,
    approval_message: null,
    approval_fields: null,
  };
}

function mkJob(partial: Partial<JobDetail> & { steps: JobStep[] }): JobDetail {
  return {
    job_id: "00000000-0000-0000-0000-000000000001",
    workspace: "default",
    task_name: "build",
    mode: "distributed",
    input: null,
    raw_input: null,
    output: null,
    status: partial.status ?? "running",
    source_type: "api",
    source_id: null,
    source_job_id: null,
    restart_from_step: null,
    revision: null,
    worker_id: null,
    created_at: new Date(NOW - 60_000).toISOString(),
    started_at: partial.started_at ?? new Date(NOW - 30_000).toISOString(),
    completed_at: null,
    retry_of_job_id: null,
    retry_job_id: null,
    retry_attempt: 0,
    max_retries: null,
    steps: partial.steps,
  };
}

function mkStats(
  taskP50: number,
  sampleSize: number,
  perStep: Array<{ name: string; p50: number }> = [],
): TaskStatsResponse {
  return {
    window: 50,
    task: {
      sample_size: sampleSize,
      avg_ms: taskP50,
      p50_ms: taskP50,
      p95_ms: taskP50 * 2,
      min_ms: taskP50 * 0.5,
      max_ms: taskP50 * 3,
      recent: [],
    },
    steps: perStep.map((s) => ({
      step_name: s.name,
      sample_size: sampleSize,
      avg_ms: s.p50,
      p50_ms: s.p50,
      p95_ms: s.p50 * 2,
      min_ms: s.p50 * 0.5,
      max_ms: s.p50 * 3,
    })),
  };
}

// ---------------------------------------------------------------------------
// preconditions (these MUST pass regardless of algorithm choice)
// ---------------------------------------------------------------------------

describe("computeEta — preconditions", () => {
  it("returns null when stats is null", () => {
    const job = mkJob({ steps: [] });
    expect(computeEta({ job, stats: null, now: NOW })).toBeNull();
  });

  it("returns null when sample size is below minSample", () => {
    const job = mkJob({ steps: [] });
    const stats = mkStats(60_000, 3);
    expect(computeEta({ job, stats, now: NOW })).toBeNull();
  });

  it("returns null for completed / failed / cancelled jobs", () => {
    for (const status of ["completed", "failed", "cancelled", "skipped"]) {
      const job = mkJob({ steps: [], status });
      const stats = mkStats(60_000, 50);
      expect(computeEta({ job, stats, now: NOW })).toBeNull();
    }
  });

  it("returns null when task p50 is missing", () => {
    const job = mkJob({ steps: [] });
    const stats = mkStats(60_000, 50);
    stats.task.p50_ms = null;
    expect(computeEta({ job, stats, now: NOW })).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// contract: a passing implementation must return SOMETHING reasonable here
// ---------------------------------------------------------------------------

describe("computeEta — contract for running jobs", () => {
  it("returns a non-null result when running with adequate sample", () => {
    const job = mkJob({
      steps: [mkStep({ step_name: "build", status: "running",
        started_at: new Date(NOW - 10_000).toISOString() })],
    });
    const stats = mkStats(60_000, 50, [{ name: "build", p50: 60_000 }]);
    expect(computeEta({ job, stats, now: NOW })).not.toBeNull();
  });

  it("reports overrun when elapsed exceeds the reference p50", () => {
    // Job started 90s ago, task p50 is 60s → 30s overrun.
    const job = mkJob({
      started_at: new Date(NOW - 90_000).toISOString(),
      steps: [mkStep({ step_name: "build", status: "running",
        started_at: new Date(NOW - 90_000).toISOString() })],
    });
    const stats = mkStats(60_000, 50, [{ name: "build", p50: 60_000 }]);
    const result = computeEta({ job, stats, now: NOW });
    expect(result?.type).toBe("overrun");
    if (result?.type === "overrun") {
      expect(result.overrunMs).toBeGreaterThan(0);
      expect(result.referenceMs).toBeGreaterThan(0);
    }
  });

  it("returns a positive remainingMs when elapsed is well under p50", () => {
    // Job started 5s ago, p50 is 60s → ~55s remaining.
    const job = mkJob({
      started_at: new Date(NOW - 5_000).toISOString(),
      steps: [mkStep({ step_name: "build", status: "running",
        started_at: new Date(NOW - 5_000).toISOString() })],
    });
    const stats = mkStats(60_000, 50, [{ name: "build", p50: 60_000 }]);
    const result = computeEta({ job, stats, now: NOW });
    expect(result?.type).toBe("eta");
    if (result?.type === "eta") {
      expect(result.remainingMs).toBeGreaterThan(0);
    }
  });
});

// ---------------------------------------------------------------------------
// indexSteps helper
// ---------------------------------------------------------------------------

describe("indexSteps", () => {
  it("includes only steps with a non-null p50", () => {
    const stats = mkStats(60_000, 50, [
      { name: "a", p50: 10_000 },
      { name: "b", p50: 20_000 },
    ]);
    stats.steps.push({
      step_name: "c",
      sample_size: 50,
      avg_ms: null,
      p50_ms: null,
      p95_ms: null,
      min_ms: null,
      max_ms: null,
    });
    const idx = indexSteps(stats);
    expect(idx.size).toBe(2);
    expect(idx.get("a")).toBe(10_000);
    expect(idx.get("b")).toBe(20_000);
    expect(idx.has("c")).toBe(false);
  });
});
