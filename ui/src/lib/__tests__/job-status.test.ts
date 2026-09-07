import { describe, it, expect } from "vitest";
import { isTerminalJobStatus, isTopLevelJob } from "../job-status";

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
