import { describe, it, expect } from "vitest";
import { deferredRefLabel, deferredRefTitle, formatPin, stepPin } from "../job-ref";

describe("deferredRefLabel", () => {
  it("labels a deferred step with its template", () => {
    expect(deferredRefLabel({ action_type: "deferred", ref_template: "{{ input.branch }}" })).toBe(
      "ref: {{ input.branch }}",
    );
  });
  it("is null once the step is resolved, or without a template", () => {
    expect(deferredRefLabel({ action_type: "script", ref_template: "{{ input.branch }}" })).toBeNull();
    expect(deferredRefLabel({ action_type: "deferred", ref_template: null })).toBeNull();
    expect(deferredRefLabel({ action_type: "deferred" })).toBeNull();
  });
});

describe("deferredRefTitle", () => {
  it("says the ref is chosen later while the step waits", () => {
    for (const status of ["pending", "ready"]) {
      expect(deferredRefTitle({ status })).toBe(
        "The branch, tag or commit is chosen when the step starts",
      );
    }
  });
  it("says the ref was not resolved on a terminal deferred row", () => {
    for (const status of ["completed", "failed", "skipped", "cancelled"]) {
      expect(deferredRefTitle({ status })).toBe(
        "This step's ref was a template; it was not resolved in this job",
      );
    }
  });
});

describe("formatPin", () => {
  it("renders ref and short commit", () => {
    expect(formatPin("release/2.3", "3f2a9c0e1b2c3d4e5f60718293a4b5c6d7e8f901")).toBe(
      "@ release/2.3 · 3f2a9c0",
    );
  });
  it("renders the ref alone without a commit", () => {
    expect(formatPin("v4.1.0", null)).toBe("@ v4.1.0");
  });
  it("is null for an unpinned job", () => {
    expect(formatPin(null, "3f2a9c0e")).toBeNull();
    expect(formatPin(undefined, undefined)).toBeNull();
  });
});

describe("stepPin", () => {
  const base = { action_ref: null, action_revision: null, task_ref: null, task_revision: null };
  it("prefers the action pin", () => {
    expect(stepPin({ ...base, action_ref: "release/1", action_revision: "abc1234def" })).toEqual({
      ref: "release/1",
      commit: "abc1234def",
    });
  });
  it("falls back to the task pin of a type: task step", () => {
    expect(stepPin({ ...base, task_ref: "v1.0.0", task_revision: "fff0000" })).toEqual({
      ref: "v1.0.0",
      commit: "fff0000",
    });
  });
  it("is null for an unpinned step", () => {
    expect(stepPin(base)).toBeNull();
  });
});
