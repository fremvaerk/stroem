import { describe, it, expect } from "vitest";
import { skipBadgeLabel, skipExplanation } from "../skip-reason";

describe("skipBadgeLabel", () => {
  it("maps every reason to a short label", () => {
    expect(skipBadgeLabel("condition", false)).toBe("condition");
    expect(skipBadgeLabel("empty", false)).toBe("empty loop");
    expect(skipBadgeLabel("cascade", false)).toBe("upstream skipped");
    expect(skipBadgeLabel("unreachable", false)).toBe("not satisfied");
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
    // Exact match, not toContain: this reason describes a step that did NOT
    // run because its dependency LACKED continue_when_skipped — the lack
    // caused the block. A substring check (e.g. "pre-0.18 only" + "a
    // dependency was itself skipped") can't distinguish that from an
    // inverted claim that the flag's presence let the step run, which is
    // the opposite of what a `cascade`-reason row actually means
    // (`gate.rs::verdict`'s `BlockSkip` branch fires on `cws == false`).
    expect(skipExplanation("cascade")).toBe(
      "Skipped (pre-0.18 only): a dependency was itself skipped, and that dependency's own lack of continue_when_skipped kept this step from running. New rows no longer use this reason — see the 0.18 upgrade guide.",
    );
    expect(skipExplanation("unreachable")).toContain("did not satisfy this step's own depends_on condition");
    expect(skipExplanation(null)).toBe("Skipped.");
  });
});
