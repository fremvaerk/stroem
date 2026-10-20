import { describe, it, expect } from "vitest";
import { missedLabel, triggersWithMissed } from "../trigger-status";
import type { TriggerInfo } from "../types";

const first = "2026-10-20T06:00:00Z";

function t(name: string, missed?: TriggerInfo["missed"]): TriggerInfo {
  return {
    name,
    type: "scheduler",
    cron: "0 6 * * *",
    task: "x",
    enabled: true,
    input: {},
    next_runs: [],
    missed,
  };
}

describe("trigger-status", () => {
  it("labels a missed count", () => {
    expect(missedLabel({ count: 3, capped: false, first })).toMatch(/^3 missed · /);
  });
  it("labels a capped count", () => {
    expect(missedLabel({ count: 1000, capped: true, first })).toMatch(/^1000\+ missed · /);
  });
  it("keeps only triggers with missed", () => {
    const a = t("a", { count: 1, capped: false, first });
    const out = triggersWithMissed([a, t("b"), t("c", null)]);
    expect(out).toEqual([a]);
  });
});
