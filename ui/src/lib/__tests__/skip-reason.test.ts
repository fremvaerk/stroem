import { describe, it, expect } from "vitest";
import { skipBadgeLabel, skipExplanation } from "../skip-reason";

describe("skipBadgeLabel", () => {
  it("maps every reason to a short label", () => {
    expect(skipBadgeLabel("condition", false)).toBe("condition");
    expect(skipBadgeLabel("empty", false)).toBe("empty loop");
    expect(skipBadgeLabel("cascade", false)).toBe("upstream skipped");
    expect(skipBadgeLabel("unreachable", false)).toBe("upstream failed");
  });

  it("falls back to the when heuristic for rows without a reason", () => {
    expect(skipBadgeLabel(null, true)).toBe("condition");
    expect(skipBadgeLabel(null, false)).toBeNull();
  });
});

describe("skipExplanation", () => {
  it("explains each reason in one sentence", () => {
    expect(skipExplanation("condition")).toContain("when condition was false");
    expect(skipExplanation("empty")).toContain("no items");
    expect(skipExplanation("cascade")).toContain("every dependency was skipped");
    expect(skipExplanation("unreachable")).toContain("upstream step failed");
    expect(skipExplanation(null)).toBe("Skipped.");
  });
});
