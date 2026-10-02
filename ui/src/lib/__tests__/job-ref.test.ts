import { describe, it, expect } from "vitest";
import { formatPin, stepPin } from "../job-ref";

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
