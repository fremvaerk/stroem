import { describe, it, expect } from "vitest";
import {
  LATER_RUNS_SHOWN,
  POLL_ACTIVE_MS,
  POLL_LATER_RUN_MS,
  POLL_LIVE_MS,
  POLL_SETTLED_MS,
  isTerminalJobStatus,
  isTopLevelJob,
  jobLineage,
  jobPollInterval,
  laterRuns,
} from "../job-status";
import type { DerivedJobRef } from "../types";

describe("isTerminalJobStatus", () => {
  it("accepts the three statuses a job actually settles at", () => {
    expect(isTerminalJobStatus("completed")).toBe(true);
    expect(isTerminalJobStatus("failed")).toBe(true);
    expect(isTerminalJobStatus("cancelled")).toBe(true);
  });

  it("rejects live statuses and missing values", () => {
    expect(isTerminalJobStatus("running")).toBe(false);
    expect(isTerminalJobStatus("pending")).toBe(false);
    expect(isTerminalJobStatus(undefined)).toBe(false);
    expect(isTerminalJobStatus(null)).toBe(false);
  });
});

describe("isTopLevelJob", () => {
  it("accepts user-initiated runs", () => {
    for (const source_type of [
      "api",
      "user",
      "trigger",
      "webhook",
      "mcp",
      "retry",
      "rerun",
      "restart",
      "event_source",
    ]) {
      expect(isTopLevelJob({ parent_job_id: null, source_type })).toBe(true);
    }
  });

  it("rejects derived source types", () => {
    for (const source_type of ["hook", "task", "agent_tool", "upload"]) {
      expect(isTopLevelJob({ parent_job_id: null, source_type })).toBe(false);
    }
  });

  it("rejects any job with a parent, whatever its source type", () => {
    expect(isTopLevelJob({ parent_job_id: "abc", source_type: "api" })).toBe(
      false,
    );
  });
});

describe("jobLineage", () => {
  const base = {
    source_type: "user",
    source_job_id: null,
    restart_from_step: null,
    retry_of_job_id: null,
    parent_job_id: null,
    parent_step_name: null,
  };

  it("points a re-run at the job it was re-run from", () => {
    expect(
      jobLineage({ ...base, source_type: "rerun", source_job_id: "src" }),
    ).toEqual({ kind: "rerun", jobId: "src", step: null });
  });

  it("carries the step a restart began at", () => {
    expect(
      jobLineage({
        ...base,
        source_type: "restart",
        source_job_id: "src",
        restart_from_step: "build",
      }),
    ).toEqual({ kind: "restart", jobId: "src", step: "build" });
  });

  it("points a task retry at the first attempt, not the previous one", () => {
    expect(
      jobLineage({ ...base, source_type: "retry", retry_of_job_id: "prev" }),
    ).toEqual({ kind: "retry", jobId: "prev", step: null });
  });

  it("points a type: task child at its parent job and the step that started it", () => {
    expect(
      jobLineage({
        ...base,
        source_type: "task",
        parent_job_id: "parent",
        parent_step_name: "deploy",
      }),
    ).toEqual({ kind: "child", jobId: "parent", step: "deploy" });
  });

  it("treats an agent tool call as a child of the agent's job", () => {
    expect(
      jobLineage({
        ...base,
        source_type: "agent_tool",
        parent_job_id: "parent",
        parent_step_name: "ask",
      }),
    ).toEqual({ kind: "child", jobId: "parent", step: "ask" });
  });

  it("points a hook job at the job that fired it", () => {
    expect(
      jobLineage({ ...base, source_type: "hook", source_job_id: "fired-by" }),
    ).toEqual({ kind: "hook", jobId: "fired-by", step: null });
  });

  it("is null for a job that did not come from another job", () => {
    expect(jobLineage(base)).toBeNull();
  });

  it("is null when the source type names lineage the row does not carry", () => {
    expect(jobLineage({ ...base, source_type: "rerun" })).toBeNull();
    expect(jobLineage({ ...base, source_type: "retry" })).toBeNull();
    // A hook job written before migration 048 whose source job is gone.
    expect(jobLineage({ ...base, source_type: "hook" })).toBeNull();
  });

  it("ignores a stray source_job_id on a non-lineage source type", () => {
    expect(
      jobLineage({ ...base, source_type: "user", source_job_id: "src" }),
    ).toBeNull();
  });
});

describe("laterRuns", () => {
  const run = (id: string, created_at: string): DerivedJobRef => ({
    id,
    source_type: "rerun",
    status: "completed",
    created_at,
    restart_from_step: null,
  });

  it("is empty when nothing was derived, or the server is older", () => {
    expect(laterRuns([])).toEqual({ shown: [], more: 0 });
    expect(laterRuns(undefined)).toEqual({ shown: [], more: 0 });
  });

  it("orders newest first whatever the input order", () => {
    const runs = [
      run("old", "2026-10-22T07:00:00Z"),
      run("new", "2026-10-22T09:00:00Z"),
      run("mid", "2026-10-22T08:00:00.5Z"),
    ];
    expect(laterRuns(runs).shown.map((r) => r.id)).toEqual(["new", "mid", "old"]);
  });

  it("compares times, not strings: fractional seconds of other lengths", () => {
    // As strings "…00Z" sorts after "…00.5Z"; as times it is earlier.
    const runs = [run("whole", "2026-10-22T07:00:00Z"), run("half", "2026-10-22T07:00:00.5Z")];
    expect(laterRuns(runs).shown.map((r) => r.id)).toEqual(["half", "whole"]);
  });

  it("keeps the server order for runs in the same millisecond", () => {
    const runs = [
      run("first", "2026-10-22T07:00:00.123456Z"),
      run("second", "2026-10-22T07:00:00.123123Z"),
    ];
    expect(laterRuns(runs).shown.map((r) => r.id)).toEqual(["first", "second"]);
  });

  it(`shows at most ${LATER_RUNS_SHOWN} and counts the rest`, () => {
    const runs = Array.from({ length: 7 }, (_, i) =>
      run(`r${i}`, `2026-10-22T07:0${i}:00Z`),
    );
    const later = laterRuns(runs);
    expect(later.shown.map((r) => r.id)).toEqual(["r6", "r5", "r4", "r3", "r2"]);
    expect(later.more).toBe(2);
  });

  it("does not count when the runs fit", () => {
    const runs = Array.from({ length: LATER_RUNS_SHOWN }, (_, i) =>
      run(`r${i}`, `2026-10-22T07:0${i}:00Z`),
    );
    expect(laterRuns(runs).more).toBe(0);
    expect(laterRuns(runs).shown).toHaveLength(LATER_RUNS_SHOWN);
  });
});

describe("jobPollInterval", () => {
  const NOW = Date.parse("2026-10-22T12:00:00Z");
  type PollJob = Parameters<typeof jobPollInterval>[0];
  const pollJob = (overrides: Partial<PollJob> = {}): PollJob => ({
    status: "failed",
    steps: [{ status: "failed", retry_at: null }],
    parent_job_id: null,
    source_type: "api",
    derived_jobs: [],
    ...overrides,
  });
  const derived = (status: string): DerivedJobRef => ({
    id: "d",
    source_type: "restart",
    status,
    created_at: "2026-10-22T11:00:00Z",
    restart_from_step: "build",
  });

  it("polls fast while a step runs or waits for approval", () => {
    for (const status of ["running", "suspended"]) {
      const job = pollJob({ status: "running", steps: [{ status, retry_at: null }] });
      expect(jobPollInterval(job, NOW)).toBe(POLL_ACTIVE_MS);
    }
  });

  it("polls at the live rate for a pending or running job with no active step", () => {
    for (const status of ["pending", "running"]) {
      const job = pollJob({ status, steps: [{ status: "ready", retry_at: null }] });
      expect(jobPollInterval(job, NOW)).toBe(POLL_LIVE_MS);
    }
  });

  it("keeps the live rate while a step retry is still due", () => {
    const due = pollJob({ steps: [{ status: "ready", retry_at: "2026-10-22T12:00:05Z" }] });
    expect(jobPollInterval(due, NOW)).toBe(POLL_LIVE_MS);
    const past = pollJob({ steps: [{ status: "failed", retry_at: "2026-10-22T11:59:55Z" }] });
    expect(jobPollInterval(past, NOW)).toBe(POLL_SETTLED_MS);
  });

  it("keeps a slow poll on a settled top-level job, for its later runs", () => {
    for (const status of ["completed", "failed", "cancelled"]) {
      expect(jobPollInterval(pollJob({ status }), NOW)).toBe(POLL_SETTLED_MS);
    }
    const done = pollJob({ derived_jobs: [derived("completed"), derived("failed")] });
    expect(jobPollInterval(done, NOW)).toBe(POLL_SETTLED_MS);
  });

  it("polls faster while a restart or re-run of the settled job is live", () => {
    for (const status of ["pending", "running"]) {
      const job = pollJob({ derived_jobs: [derived("completed"), derived(status)] });
      expect(jobPollInterval(job, NOW)).toBe(POLL_LATER_RUN_MS);
    }
  });

  it("stops for a settled job that can get no later runs", () => {
    const child = pollJob({ parent_job_id: "p", source_type: "task" });
    expect(jobPollInterval(child, NOW)).toBeNull();
    const hook = pollJob({ source_type: "hook" });
    expect(jobPollInterval(hook, NOW)).toBeNull();
  });

  it("stops for a settled job from an older server that sends no derived_jobs", () => {
    expect(jobPollInterval(pollJob({ derived_jobs: undefined }), NOW)).toBeNull();
  });
});
