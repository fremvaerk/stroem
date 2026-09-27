import { describe, it, expect } from "vitest";
import { isTerminalJobStatus, isTopLevelJob, jobLineage } from "../job-status";

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
